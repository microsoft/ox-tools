// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn decision(value: bool) -> bool {
    value
}

fn retry(ready: bool) {
    while ready {
        let reduced = decision(false);
        if reduced {
            continue;
        }
        break;
    }
}

fn fresh_proof(remaining: &mut u32) -> bool {
    *remaining -= 1;
    false
}

fn budget(ready: bool, mut remaining: u32) {
    while ready {
        let reduced = fresh_proof(&mut remaining);
        if reduced {
            continue;
        }
        break;
    }
}

fn memoized(cache: &std::collections::HashMap<u32, bool>, budget: &mut u32, key: u32) -> bool {
    if let Some(cached) = cache.get(&key) {
        return *cached;
    }
    *budget += 1;
    false
}

fn warm_retry(ready: bool) {
    let cache: std::collections::HashMap<u32, bool> = std::collections::HashMap::from([(7_u32, false)]);
    let mut budget: u32 = 0;
    while ready {
        let reduced = memoized(&cache, &mut budget, 7_u32);
        if reduced {
            continue;
        }
        break;
    }
}

fn fresh_retry(ready: bool) {
    let cache: std::collections::HashMap<u32, bool> = std::collections::HashMap::from([(7_u32, false)]);
    let mut budget: u32 = 0;
    while ready {
        let reduced = memoized(&cache, &mut budget, 8_u32);
        if reduced {
            continue;
        }
        break;
    }
}

fn changed_population(ready: bool, key: u32) {
    let cache: std::collections::HashMap<u32, bool> = std::collections::HashMap::from([(7_u32, false)]);
    let mut budget: u32 = 0;
    while ready {
        let reduced = memoized(&cache, &mut budget, key);
        if reduced {
            continue;
        }
        break;
    }
}

fn counted(cache: &std::collections::HashMap<u32, bool>, budget: &mut u32, key: u32) -> bool {
    *budget += 1;
    if let Some(cached) = cache.get(&key) {
        return *cached;
    }
    false
}

fn budget_before_cache(ready: bool) {
    let cache: std::collections::HashMap<u32, bool> = std::collections::HashMap::from([(7_u32, false)]);
    let mut budget: u32 = 0;
    while ready {
        let reduced = counted(&cache, &mut budget, 7_u32);
        if reduced {
            continue;
        }
        break;
    }
}

fn fallible(cache: &std::collections::HashMap<u32, bool>, budget: &mut u32, key: u32) -> Result<bool, ()> {
    if *budget == 0 {
        return Err(());
    }
    Ok(memoized(cache, budget, key))
}

fn error_retry(ready: bool) -> Result<(), ()> {
    let cache: std::collections::HashMap<u32, bool> = std::collections::HashMap::from([(7_u32, false)]);
    let mut budget: u32 = 0;
    while ready {
        let reduced = fallible(&cache, &mut budget, 7_u32)?;
        if reduced {
            continue;
        }
        break;
    }
    Ok(())
}
