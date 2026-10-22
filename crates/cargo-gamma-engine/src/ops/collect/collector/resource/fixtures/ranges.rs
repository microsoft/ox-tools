// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn positive(n: u32, mut key: u64) {
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn changing_guard(n: u64, mut key: u64) {
    let mut next: i64 = 0;
    while key < n {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn unknown_float(n: u32, divisor: f64) {
    let mut next: i64 = 0;
    while next < i64::from(n) {
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn opaque_configuration(n: u32, mut key: u64, divisor: f64) {
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        #[cfg(any())]
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn typed_shadow(n: u32) {
    let key = key_source();
    let mut key: foreign::Word = foreign::Word(0);
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn tuple_shadow(n: u32) {
    let key = key_source();
    let (mut key,) = (foreign::Word(0),);
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn foreign_return(n: u32) {
    let mut key = foreign::create();
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
    }
}

fn shadowed_default(n: u32, mut key: u64) {
    use foreign::Default;
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
        let _: i64 = Default::default();
    }
}

fn key_source() -> u64 {
    0
}

trait Maker {
    fn default() -> i64;
}

fn generic_default<Default: Maker>(n: u32, mut key: u64) {
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
        let _: i64 = Default::default();
    }
}

fn module_default(n: u32, mut key: u64) {
    #[allow(non_snake_case)]
    mod Default {
        pub fn default() -> i64 {
            1
        }
    }
    let mut next: i64 = 0;
    while next < i64::from(n) {
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        let divisor = ((key >> 33).wrapping_add(1)) as f64;
        let scaled = f64::from(1_u32 << 31) / divisor;
        next = scaled as i64;
        let _: i64 = Default::default();
    }
}

mod foreign {
    #[derive(Clone, Copy)]
    pub struct Word(pub u8);

    #[allow(non_camel_case_types)]
    type u64 = Word;

    pub fn create() -> u64 {
        Word(0)
    }

    impl Word {
        pub fn wrapping_mul(self, _: core::primitive::u64) -> Self {
            Self(self.0.saturating_add(1))
        }

        pub fn wrapping_add(self, _: core::primitive::u64) -> Self {
            self
        }
    }

    impl core::ops::Shr<u32> for Word {
        type Output = core::primitive::u64;

        fn shr(self, _: u32) -> Self::Output {
            assert!(self.0 < 2);
            0
        }
    }

    pub struct Default;

    impl Default {
        pub fn default() -> i64 {
            1
        }
    }
}
