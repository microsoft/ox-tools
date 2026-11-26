// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use compact_str::CompactString;
use jiff::Timestamp;

#[derive(Debug, Clone)]
pub enum MetricValue {
    UInt(u64),
    Float(f64),
    String(CompactString),
    Boolean(bool),
    DateTime(Timestamp),
    List(Vec<Self>),
}
