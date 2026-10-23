// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::{BufReader, Cursor, Read};
use std::option::Option::Some as Imported;
use std::time::Duration as Span;

pub mod missing {
    pub struct Artifact;
    pub fn local() -> Artifact {
        Artifact
    }
}
pub mod ready {
    #[derive(Default)]
    pub struct Artifact;
    pub fn local() -> Artifact {
        Artifact
    }
}
pub mod catalog {
    pub use super::missing::Artifact;
}
pub use missing::Artifact as Renamed;
pub type Linked = Renamed;
pub struct ThroughAlias;
pub type WithDefault = ThroughAlias;
impl Default for WithDefault {
    fn default() -> Self {
        Self
    }
}
#[cfg_attr(all(), derive(Default))]
pub struct Configured;
pub fn configured() -> Configured {
    Configured
}
pub fn alias_defaulted() -> ThroughAlias {
    ThroughAlias
}

pub fn qualified() -> self::missing::Artifact {
    missing::Artifact
}
pub fn renamed() -> Renamed {
    missing::Artifact
}
pub fn linked() -> Linked {
    missing::Artifact
}
pub fn reexported() -> catalog::Artifact {
    missing::Artifact
}
pub fn empty() -> Vec<Renamed> {
    vec![missing::Artifact]
}
pub fn wrapper() -> Box<Renamed> {
    Box::new(missing::Artifact)
}
pub fn empty_wrapper() -> Box<Vec<Renamed>> {
    Box::new(vec![missing::Artifact])
}
pub fn break_value() -> Renamed {
    loop {
        break missing::Artifact;
    }
}
pub fn optional() -> Option<Renamed> {
    Some(missing::Artifact)
}
pub fn defaulted() -> ready::Artifact {
    ready::Artifact
}
pub struct Times {
    pub span: Span,
}
pub struct Numbers {
    pub span: u32,
}
pub fn duration() -> Result<Span, ()> {
    Ok(Span::from_secs(4))
}
pub fn field_duration(value: Times, scalar: u32) -> Span {
    value.span * scalar
}
pub fn field_numeric(value: Numbers, scalar: u32) -> u32 {
    value.span * scalar
}
pub fn propagated(scalar: u32) -> Result<Span, ()> {
    Ok(duration()? * scalar)
}
pub fn duration_pair(left: Span, right: Span) -> Span {
    left + right
}
pub fn duration_division(left: Span, scalar: u32) -> Span {
    left / scalar
}
pub fn text(value: String, suffix: &str) -> String {
    value + suffix
}
pub fn reader(value: Cursor<Vec<u8>>) -> Vec<u8> {
    let _read = value.take(3);
    Vec::new()
}
pub fn buffered(value: BufReader<Cursor<Vec<u8>>>) -> Vec<u8> {
    let _read = value.take(3);
    Vec::new()
}
pub fn reader_chain(value: Cursor<Vec<u8>>) -> Vec<u8> {
    let _read = value.take(3).take(2);
    Vec::new()
}
pub fn opened(path: &std::path::Path) -> std::io::Result<()> {
    let _read = std::fs::File::open(path)?.take(3);
    Ok(())
}
pub fn iterator(value: Vec<u8>) -> impl Iterator<Item = u8> {
    value.into_iter().take(3)
}
pub enum Custom {
    Some(u8),
    None,
}
pub fn custom_some() -> Custom {
    Custom::Some(1)
}
pub fn custom_imported(present: bool) -> Custom {
    use Custom::{None, Some};
    if present { Some(1) } else { None }
}
pub fn standard_some() -> Option<u8> {
    Option::Some(1)
}
pub fn alias_some() -> Option<u8> {
    use std::option::Option::Some as Present;
    Present(1)
}
pub fn imported_some() -> Option<u8> {
    Imported(1)
}
pub struct CustomApi;
impl CustomApi {
    pub fn take(self, count: usize) -> std::iter::Take<std::ops::Range<usize>> {
        (0..5).take(count)
    }
    pub fn skip(self, count: usize) -> std::iter::Skip<std::ops::Range<usize>> {
        (0..5).skip(count)
    }
}
pub fn custom_api() -> impl Iterator<Item = usize> {
    CustomApi.take(2)
}

macro_rules! not_implemented {
    ($ty:ty, $bound:path) => {
        const _: fn() = || {
            trait Ambiguous<A> {
                fn check() {}
            }
            impl<T: ?Sized> Ambiguous<()> for T {}
            impl<T: ?Sized + $bound> Ambiguous<u8> for T {}
            let _ = <$ty as Ambiguous<_>>::check;
        };
    };
}

not_implemented!(missing::Artifact, Default);
not_implemented!(std::process::Output, Default);
not_implemented!(std::process::Command, Default);
not_implemented!(std::process::Stdio, Default);
not_implemented!(std::process::Child, Default);
not_implemented!(std::time::Instant, Default);
not_implemented!(std::time::SystemTime, Default);
not_implemented!(std::cmp::Ordering, Default);
not_implemented!(std::fs::File, Default);
not_implemented!(std::sync::MutexGuard<'static, ()>, Default);
not_implemented!(std::sync::RwLockReadGuard<'static, ()>, Default);
not_implemented!(std::sync::RwLockWriteGuard<'static, ()>, Default);
not_implemented!(std::sync::Barrier, Default);
not_implemented!(std::thread::JoinHandle<()>, Default);
not_implemented!(std::sync::mpsc::Sender<u8>, Default);
not_implemented!(std::sync::mpsc::SyncSender<u8>, Default);
not_implemented!(std::sync::mpsc::Receiver<u8>, Default);
not_implemented!(serde_json::Error, Default);
not_implemented!(syn::Error, Default);
not_implemented!(std::io::Error, Default);
not_implemented!(std::num::ParseIntError, Default);
not_implemented!(std::num::ParseFloatError, Default);
not_implemented!(std::num::TryFromIntError, Default);
not_implemented!(std::str::Utf8Error, Default);
not_implemented!(std::string::FromUtf8Error, Default);
not_implemented!(std::string::FromUtf16Error, Default);
not_implemented!(std::env::VarError, Default);
not_implemented!(std::env::JoinPathsError, Default);
not_implemented!(std::net::AddrParseError, Default);
not_implemented!(Span, std::ops::Mul<Span>);
not_implemented!(Span, std::ops::Div<Span>);
not_implemented!(Span, std::ops::Rem<u32>);
not_implemented!(String, std::ops::Sub<&'static str>);
not_implemented!(String, std::ops::Mul<&'static str>);

pub fn compiled_mutants(left: Span, right: Span, scalar: u32) -> (Span, Span, f64, Vec<Renamed>, Option<Renamed>, usize) {
    let divided = left / scalar;
    let subtracted = left - right;
    let ratio = left.as_secs_f64() / right.as_secs_f64();
    let empty = Vec::new();
    let none = None;
    let skipped = [1, 2, 3].into_iter().skip(1).count();
    (divided, subtracted, ratio, empty, none, skipped)
}
