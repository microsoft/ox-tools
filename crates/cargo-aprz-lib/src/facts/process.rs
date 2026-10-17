// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::io;

pub(crate) enum RunError {
    Spawn(io::Error),
    Wait(io::Error),
    Timeout,
}

pub(crate) async fn run_with_timeout<O, F, T>(
    timeout: Duration,
    spawn: impl FnOnce(bool) -> io::Result<F>,
    wait: impl FnOnce(Duration, F) -> T,
) -> Result<O, RunError>
where
    F: Future<Output = io::Result<O>>,
    T: Future<Output = Result<io::Result<O>, ()>>,
{
    let child = spawn(true).map_err(RunError::Spawn)?;
    wait(timeout, child).await.map_err(|()| RunError::Timeout)?.map_err(RunError::Wait)
}

pub(crate) async fn wait_with_timeout<O>(timeout: Duration, child: impl Future<Output = io::Result<O>>) -> Result<io::Result<O>, ()> {
    tokio::time::timeout(timeout, child).await.map_err(|_elapsed| ())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) mod test_util {
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    pub(crate) struct ScriptedChild<O> {
        pub(crate) output: Option<O>,
        pub(crate) completed: Rc<Cell<bool>>,
        pub(crate) dropped: Rc<Cell<bool>>,
    }

    impl<O: Unpin> Future for ScriptedChild<O> {
        type Output = io::Result<O>;

        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            if let Some(output) = self.output.take() {
                self.completed.set(true);
                Poll::Ready(Ok(output))
            } else {
                Poll::Pending
            }
        }
    }

    impl<O> Drop for ScriptedChild<O> {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    pub(crate) fn expire<O: Unpin + Send>(timeout: Duration, child: ScriptedChild<O>) -> std::future::Ready<Result<io::Result<O>, ()>> {
        assert_eq!(timeout, Duration::from_secs(10));
        let mut child = Box::pin(child);
        assert!(child.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        drop(child);
        std::future::ready(Err(()))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::cell::Cell;
    use std::rc::Rc;

    use super::test_util::ScriptedChild;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn real_timeout_drops_a_pending_child_without_completing_it() {
        let completed = Rc::new(Cell::new(false));
        let dropped = Rc::new(Cell::new(false));
        let child = ScriptedChild::<()> {
            output: None,
            completed: Rc::clone(&completed),
            dropped: Rc::clone(&dropped),
        };
        let mut waiting = pin!(wait_with_timeout(Duration::from_secs(10), child));
        let mut context = Context::from_waker(Waker::noop());

        assert!(waiting.as_mut().poll(&mut context).is_pending());
        assert!(!completed.get());
        assert!(!dropped.get());

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(waiting.as_mut().poll(&mut context).is_pending());
        assert!(!completed.get());
        assert!(!dropped.get());

        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(matches!(waiting.as_mut().poll(&mut context), Poll::Ready(Err(()))));
        assert!(!completed.get());
        assert!(
            dropped.get(),
            "the production timeout must drop the child before returning its error"
        );
    }
}
