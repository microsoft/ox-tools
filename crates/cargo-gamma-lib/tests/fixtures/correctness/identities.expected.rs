// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub struct TestBinary;

pub struct Judge;

impl Judge {
    fn judge_ranked(&self, _binary: &TestBinary, flag: bool) -> bool {
        flag
    }
}

pub fn qualified(binary: &TestBinary, flag: bool) -> bool {
    Judge::judge_ranked(&Judge, binary, (if ::gamma_rt::a(1u32) { !(flag) } else { flag }))
}

pub fn numeric(value: usize) -> usize {
    let file = unknown(value);
    take((if ::gamma_rt::a(2u32) { (file) + 1 } else { file }));
    let _ = file - 1;
    let file = String::new();
    hold(file);
    file_len(value)
}

fn unknown<T>(value: T) -> T {
    value
}

fn take(value: usize) -> usize {
    value
}

fn hold(_value: String) {}

fn file_len(value: usize) -> usize {
    value
}

pub fn closure(file: usize) -> usize {
    let convert = |file: &str| file.len();
    convert("x") + file
}

pub fn borrowed_boolean((flag,): (&bool,)) -> bool {
    (if ::gamma_rt::a(3u32) { *(flag) } else { !flag })
}

pub fn payload<'a>(root: &'a std::path::Path) -> Option<&'a std::path::Path> {
    if root.is_absolute() {
        (if ::gamma_rt::a(4u32) { Some(root) } else { None })
    } else {
        Some(root)
    }
}

pub fn shadowed<'a>(root: &'a std::path::Path) -> Option<&'a std::path::Path> {
    let root = root.to_path_buf();
    if root.is_absolute() { None } else { None }
}

pub struct Endpoint {
    value: String,
}

impl Endpoint {
    pub fn unrelated(&self, other: &str) -> Option<&str> {
        let _ = other;
        if self.value.is_empty() { None } else { Some(&self.value) }
    }
}

async fn discover() -> Option<usize> {
    Some(3)
}

fn synchronous() -> Option<usize> {
    Some(5)
}

pub async fn future() -> Option<usize> {
    discover().await
}

pub fn sync_call() -> Option<usize> {
    (if ::gamma_rt::a(5u32) {
        ::gamma_rt::default_after(synchronous())
    } else {
        synchronous()
    })
}

pub fn callable(discover: impl Fn() -> String) -> String {
    discover()
}

fn disabled_parameter(#[cfg(any())] ignored: bool, text: String, flag: bool) -> usize {
    if flag { text.into_bytes().len() } else { 0 }
}

pub fn cfg_parameter(flag: bool) -> usize {
    disabled_parameter(String::from("x"), (if ::gamma_rt::a(6u32) { !(flag) } else { flag }))
}

struct Gated;

impl Gated {
    fn take(#[cfg(any())] &self, text: String, flag: bool) -> usize {
        if flag { text.into_bytes().len() } else { 0 }
    }
}

pub fn cfg_receiver(flag: bool) -> usize {
    Gated::take(String::from("xy"), (if ::gamma_rt::a(7u32) { !(flag) } else { flag }))
}
