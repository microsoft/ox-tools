// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::process::{Child, Command};

/// Kills and reaps even a suspended child that has not yet been assigned to a job.
pub(crate) struct TestChild(Child);

impl TestChild {
    pub(crate) fn spawn(command: &mut Command) -> Self {
        Self(command.spawn().expect("the owned test child starts"))
    }
}

impl core::ops::Deref for TestChild {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl core::ops::DerefMut for TestChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        let _killed = self.0.kill();
        let _reaped = self.0.wait();
    }
}
