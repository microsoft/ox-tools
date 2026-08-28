// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;

/// Cost and value for one mutator family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yield {
    /// The family name, such as `relational`.
    pub family: String,

    /// How many mutants it produced.
    pub mutants: u32,

    /// CPU time spent deciding them.
    pub cpu: Duration,

    /// How many of them survived — the only output of a mutation run that teaches anything.
    pub survivors: u32,
}

impl Yield {
    /// Survivors found per CPU-hour, the ratio that makes families comparable.
    #[must_use]
    pub fn per_cpu_hour(&self) -> f64 {
        let hours = self.cpu.as_secs_f64() / 3600.0;

        if hours <= 0.0 {
            return 0.0;
        }

        f64::from(self.survivors) / hours
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_family_with_no_time_has_no_ratio_rather_than_an_infinite_one() {
        let row = Yield {
            family: "x".to_owned(),
            mutants: 1,
            cpu: Duration::ZERO,
            survivors: 1,
        };

        assert!((row.per_cpu_hour() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_ratio_is_survivors_per_cpu_hour() {
        let row = Yield {
            family: "x".to_owned(),
            mutants: 7,
            cpu: Duration::from_mins(30),
            survivors: 3,
        };
        assert!((row.per_cpu_hour() - 6.0).abs() < f64::EPSILON);
    }
}
