# Mutators

Every mutation `cargo-gamma` can apply, what each one asks of a test suite, and how to choose
between them.

A **mutator** is one transformation with one stable name of the form `family.transform`. A
**family** is the group of mutators sharing a first component. A **mutator preset** is a named set,
selected with a leading `@`. Those three words are the whole vocabulary, and they are the same
words used by `--mutators`, by suppression directives, by the reports, and by `explain`.

The mutation-testing literature calls a mutator a *mutation operator*. This document does not,
and neither does the tool: "operator" here always means a Rust operator — the `+` or the `<` that
a mutator rewrites — so that a sentence about swapping one can never be read as a sentence about
the catalog.

For the flags that select these, see [CMDLINE.md](CMDLINE.md); for the `mutators` configuration key, see
[CONFIG.md](CONFIG.md).

## Contents

* [The catalog](#the-catalog)
* [Choosing what to run](#choosing-what-to-run)
* [The families at a glance](#the-families-at-a-glance)
* [The families in detail](#the-families-in-detail)
* [Every mutator](#every-mutator)
* [What the catalog deliberately omits](#what-the-catalog-deliberately-omits)
* [Mutator presets](#mutator-presets)

## The catalog

Every mutator has one stable name of the form `family.transform`. That single name is the whole
vocabulary: it is what `--mutators` selects, what a suppression directive names, what the report prints
in brackets after each mutant, what a SARIF rule identifier is set to, and what `explain` accepts.
Nothing refers to a mutator by number or by position, so a name you write down today keeps working
as the catalog grows.

[Mutator presets](#mutator-presets) group these into named sets, and [suppressing mutations](../README.md#suppressing-mutations)
covers how to turn one off for a particular site.

## Choosing what to run

The default preset contains the main catalog. Valid mutations with evidence of low yield are kept
in the opt-in `@pedantic` preset so ordinary runs do not pay for them without asking.

A selector is a mutator name, a family prefix, a [preset](#mutator-presets), or an academic alias. `!`
removes from the set, and selectors apply left to right:

```bash
cargo gamma run --mutators relational              # one family
cargo gamma run --mutators relational.lt_to_le     # one mutator
cargo gamma run --mutators @arithmetic,!bitwise    # a preset, less one family
cargo gamma run --mutators all,!stmt               # everything except one family
cargo gamma run --mutators ROR                     # by academic alias
```

A selector that matches nothing is an error rather than a silent no-op. A filter that quietly does
nothing leaves the score high and gives nobody a reason to look.

To see the catalog as your current selection resolves it, with a `*` against each enabled mutator:

```bash
cargo gamma list mutators
cargo gamma explain relational.lt_to_le   # what one does, and how to switch it off
```

## The families at a glance



<!-- begin generated: families -->

| Family | Mutators | What it asks |
| --- | ---: | --- |
| [`fn_value`](#fn_value) | 21 | Does anything check what this function returns? |
| [`relational`](#relational) | 10 | Is this comparison's boundary the right one? |
| [`arith`](#arith) | 10 | Does this calculation's operator matter? |
| [`bitwise`](#bitwise) | 4 | Is this mask or flag combination correct? |
| [`shift`](#shift) | 2 | Is this shift's direction load-bearing? |
| [`assign`](#assign) | 10 | Does this compound assignment's operator matter? |
| [`logical`](#logical) | 2 | Is this `&&` really an `&&`? |
| [`cond`](#cond) | 3 | Does anything depend on this branch being taken? |
| [`match_guard`](#match_guard) | 3 | Does anything depend on this guard being right? |
| [`match_arm`](#match_arm) | 1 | Is this arm reachable, and does anything notice when it stops matching? |
| [`struct_field`](#struct_field) | 1 | Does this field's value matter, or is the default good enough? |
| [`range`](#range) | 2 | Is this bound inclusive on purpose? |
| [`loop`](#loop) | 4 | Does this `break` or `continue` carry the loop's meaning? |
| [`unary`](#unary) | 2 | Does this negation or complement matter? |
| [`literal`](#literal) | 7 | Does this constant's exact value matter? |
| [`stmt`](#stmt) | 2 | Does this statement's side effect matter? |
| [`expr`](#expr) | 2 | Would an off-by-one here be caught? |
| [`option`](#option) | 2 | Is the present case distinguished from the absent one? |
| [`result`](#result) | 2 | Is success distinguished from failure? |
| [`iter`](#iter) | 8 | Does anything observe that this was ordered, deduplicated, or taken from one end? |
| [`string`](#string) | 6 | Does the prefix, the case, or the trimmed end actually matter? |
| [`collection`](#collection) | 1 | Does every element of this literal earn its place? |
| [`assign_value`](#assign_value) | 1 | Is the value assigned here ever read in a way that would notice? |
| **Total** | **106** | |

<!-- end generated -->



## The families in detail

Each family below says what it targets, what a surviving mutant tells you about the suite, and what
the transformation looks like in practice. The mutator names in the examples are real catalog
entries, so any of them can be passed to `explain`.

### `fn_value`

Replaces an entire function body with a fixed value of the same type. The family covers unit and
boolean values, small signed and unsigned numbers, empty and sentinel strings, defaults, option and
result variants, tuples, empty iterators and collections, and values stated explicitly with an
attribute. A surviving mutant here says that nothing calling this function checks its return value
at all — the function could be replaced by a constant and the suite would not notice.

```rust
// original
fn shipping_zone(order: &Order) -> Zone { compute_zone(order) }

// fn_value.default
fn shipping_zone(order: &Order) -> Zone { Default::default() }

// fn_value.zero
fn item_count(order: &Order) -> u32 { 0 }

// fn_value.stated, from #[gamma::value(Zone::Domestic)] on the function
fn shipping_zone(order: &Order) -> Zone { Zone::Domestic }
```

A function returning `impl Trait` gets no mutant, and neither does anything evaluated in a `const` context — both are explained under "What the catalog deliberately omits" in the README. A site that
gets no mutant, or the wrong one, can [state the value itself](#stating-the-value-yourself).

### `relational`

Targets the boundary of a comparison: `<`, `<=`, `>`, `>=`, `==`, `!=`. A surviving mutant means the suite has no test sitting exactly on that boundary — off-by-one errors in loop bounds, capacity checks, and range tests live here.

```rust
// original
if index < limit { … }

// relational.lt_to_le
if index <= limit { … }

// relational.eq_to_ne
if remaining == 0 { … }
```

The `ROR` alias covers this whole family; a test suite that only exercises values well inside or well outside a boundary, never on it, will let all ten mutators here survive together.

### `arith`

Swaps one arithmetic operator for another: `+`/`-`/`*`/`/`/`%` interchanged in pairs. A surviving mutant means no test distinguishes the actual formula from a nearby wrong one — the inputs used never produce a different result under the substituted operator.

```rust
// original
let total = subtotal + tax;

// arith.add_to_sub
let total = subtotal - tax;

// arith.mul_to_div
let area = width / height;
```

Some substitutions are prone to equivalent mutants on specific inputs — e.g. `div_to_rem` when the dividend is always exactly divisible — so a survivor is worth reading before concluding the suite is weak.

### `bitwise`

Swaps `&`, `|`, and `^` for one another. A surviving mutant means no test distinguishes the actual flag or mask combination from a different one — typically because the tested value has bits that make several operators agree by coincidence.

```rust
// original
let masked = flags & READ_MASK;

// bitwise.and_to_or
let masked = flags | READ_MASK;

// bitwise.xor_to_and
let toggled = state ^ ENABLED;
```

### `shift`

Swaps `<<` for `>>` and back. A surviving mutant means no test cares which direction a shift moves bits — often because the shifted value is zero, or the result is only used for a property that both directions happen to satisfy.

```rust
// original
let packed = high_byte << 8;

// shift.shl_to_shr
let packed = high_byte >> 8;
```

### `assign`

The compound-assignment counterpart of `arith`/`bitwise`/`shift`: swaps `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`, `<<=`, `>>=` for their paired opposite. A surviving mutant means the test never observes the accumulated value after the compound assignment runs, only before, or in a case where both operators land on the same result.

```rust
// original
total += discount;

// assign.add_to_sub
total -= discount;

// assign.shl_to_shr
buffer <<= 1;
```

### `assign_value`

Replaces the right-hand side of a plain assignment with `Default::default()` for its type. A surviving mutant means the assigned value is set but never read in a way a test would notice — a dead store, or a value overwritten before anything observes it.

```rust
// original
self.retry_count = attempts_so_far + 1;

// assign_value.default
self.retry_count = Default::default();
```

This asks about bookkeeping fields and intermediate state at the exact point a value is decided, a question whole-function replacement (`fn_value`) cannot ask because it only ever sees one function's return at a time.

### `logical`

Swaps `&&` for `||` and back in a boolean expression. A surviving mutant means no test input makes the two operands disagree — both are true, both are false, or only one side is ever exercised at all.

```rust
// original
if is_authenticated && has_permission { … }

// logical.and_to_or
if is_authenticated || has_permission { … }
```

Because a two-operand `&&`/`||` swap needs only one input where the operands differ to kill, a survivor here is a strong, specific signal: add the case where exactly one side is true and the other false.

### `cond`

Mutates a branch condition itself, independent of what operator it contains: negating it, or forcing it to always be true or always false. A surviving `always_true`/`always_false` mutant means one arm of the branch is never taken by any test; a surviving `negate` means both arms produce output the suite cannot tell apart.

```rust
// original
if order.is_expedited() { apply_rush_fee(order) }

// cond.always_false
if false { apply_rush_fee(order) }

// cond.negate
if !order.is_expedited() { apply_rush_fee(order) }
```

### `match_guard`

The same three condition mutants — negate, always true, always false — applied to a match arm's `if` guard rather than an `if` statement. A surviving mutant means no test distinguishes an arm reached because its guard held from one reached despite it, or exercises the guard's false branch (falling through to a later arm) at all.

```rust
// original
match shipment {
    Shipment::Delayed(days) if days > 7 => escalate(),
    …
}

// match_guard.always_false
Shipment::Delayed(days) if false => escalate(),
```

An arm whose guard is forced false is not also emitted as a `match_arm.never_matches` mutant, since the two would ask the same question of the same code.

### `match_arm`

Stops one match arm from matching by rewriting its pattern to something that never does, letting execution fall through to a later wildcard arm. A surviving mutant means the suite never supplies a value that only this arm — and not the wildcard — was written to handle.

```rust
// original
match status {
    Status::Pending => queue(),
    Status::Canceled => refund(),
    _ => noop(),
}

// match_arm.never_matches (on the Pending arm)
match status {
    Status::Pending if false => queue(),
    Status::Canceled => refund(),
    _ => noop(),
}
```

Only arms before an unguarded wildcard are eligible; without one, disabling an arm would make the match non-exhaustive and the mutant could never compile.

### `loop`

Mutates loop control flow: swapping `break` for `continue` and back, or deleting either one outright. A surviving mutant means no test depends on the loop actually stopping (or actually skipping to the next iteration) at that point — the same observable outcome is reached whether or not the loop runs the extra iterations.

```rust
// original
for item in &items {
    if item.is_invalid() { continue; }
    if item.is_terminal() { break; }
    process(item);
}

// loop.continue_to_break
if item.is_invalid() { break; }

// loop.delete_break
if item.is_terminal() { }
```

### `range`

Moves a range's endpoint by one rather than rewriting `..` as `..=`, since the two literal spellings are different types and a rewrite could never type-check as one arm of the mutant's guarding `if`. A surviving mutant means no test is sensitive to whether the range's own endpoint is included.

```rust
// original
for i in 0..buffer.len() { … }

// range.exclusive_to_inclusive
for i in 0..(buffer.len() + 1) { … }

// range.inclusive_to_exclusive
for i in 0..=(last_index - 1) { … }
```

On an unsigned endpoint that is already zero, `inclusive_to_exclusive` underflows and is caught by the panic rather than by an assertion — the right result reached for an incidental reason.

### `literal`

Replaces a literal constant with a nearby one of the same kind: an integer zeroed, set to one, incremented, or decremented; a boolean flipped; a string emptied or replaced. A surviving mutant means the suite never asserts the literal's *exact* value, only that it is present, non-zero, or non-empty.

```rust
// original
const MAX_RETRIES: u32 = 3;

// literal.int_decrement
const MAX_RETRIES: u32 = 2;

// literal.bool_flip
let verbose = false;
```

### `expr`

Adds or subtracts one from a numeric expression, but only where the source itself gives evidence the value is numeric — an annotation, a typed initializer, arithmetic, indexing, or comparison against an integer literal. A surviving mutant means a boundary-sensitive value (an argument, a return, an index, a range endpoint) can be off by one without any test noticing.

```rust
// original
fn take_first(n: usize, items: &[Item]) -> &[Item] { &items[..n] }

// expr.increment
fn take_first(n: usize, items: &[Item]) -> &[Item] { &items[..(n + 1)] }
```

Deliberately narrower than the `literal` family: the two mutators only fire on evidence, not guesswork, avoiding unviable mutants on genuinely non-numeric expressions.

### `unary`

Removes a unary `-` or `!` outright, leaving the plain operand in its place. A surviving mutant means no test distinguishes a negated or inverted value from the raw one — either the operand's sign or truth value never changes what the test observes.

```rust
// original
let balance = -pending_debit;

// unary.remove_neg
let balance = pending_debit;

// unary.remove_not
if !cache.is_empty() { … }
if cache.is_empty() { … }
```

### `stmt`

Deletes an entire statement outright: a call whose value is discarded, or an assignment (plain or compound). A surviving mutant means the statement's side effect — the call it made, or the value it stored — is never observed by anything the suite checks.

```rust
// original
logger.flush();
counter += 1;

// stmt.delete_call
counter += 1;

// stmt.delete_assign
logger.flush();
```

A call whose return value is used, rather than discarded, is not a candidate here — removing it would remove the value the rest of the expression depends on, which is a different mutation than this family asks about.

### `struct_field`

Omits one field from a struct literal that has a `..base` expression, letting the base supply the field's value instead. A surviving mutant means no test can tell the value the code explicitly wrote from whatever the base struct already had in that field.

```rust
// original
let updated = Order { status: Status::Shipped, ..existing };

// struct_field.omit
let updated = Order { ..existing };
```

Only eligible when a base expression is present — without one, removing a field would leave the literal incomplete and it could not compile.

### `option`

Rewrites an `Option` construction across the present/absent boundary: `Some(v)` to `None`, or `None` to `Some(Default::default())`. A surviving mutant means the caller's handling of the present and absent cases is never actually distinguished by a test — both paths produce output the suite accepts.

```rust
// original
fn find_user(id: UserId) -> Option<User> { users.get(&id).cloned() }

// option.some_to_none
fn find_user(id: UserId) -> Option<User> { None }
```

### `result`

The same present/absent question as `option`, but for success and failure: `Ok(v)` to `Err(Default::default())`, and back. A surviving mutant means no test distinguishes the success path from the failure path — often because an error is only logged, or a `?` silently propagates without the caller's behavior ever differing.

```rust
// original
fn parse_amount(s: &str) -> Result<Decimal, ParseError> { s.parse().map_err(ParseError::from) }

// result.ok_to_err
fn parse_amount(s: &str) -> Result<Decimal, ParseError> { Err(Default::default()) }
```

### `iter`

Swaps a standard-library iterator method for a nearby one that returns the same type: `any`/`all`, `min`/`max`, `first`/`last`, and removes a `sort` or `dedup` from a chain outright. A surviving mutant means the suite never depends on the actual quantifier, extremum, end, ordering, or deduplication — for example a `min`/`max` swap survives when the tested collection has a single element.

```rust
// original
let cheapest = prices.iter().min();

// iter.min_to_max
let cheapest = prices.iter().max();

// iter.remove_sort
let ordered = { names.sort(); names };
```

Limited to a curated set of standard-library names with matching return types (`take`/`skip` and dropping a `filter` are absent because they would change the type), since without type resolution there is no way to know a user-defined `min` or `max` means what the standard library's does.

### `string`

Swaps a `str`/`String` method for its semantic opposite: `starts_with`/`ends_with`, `to_lowercase`/`to_uppercase`, `trim_start`/`trim_end`. A surviving mutant means no test depends on which end or which case direction was actually chosen.

```rust
// original
if path.ends_with(".rs") { … }

// string.ends_with_to_starts_with
if path.starts_with(".rs") { … }

// string.trim_start_to_trim_end
let cleaned = raw.trim_end();
```

The text inside `expect`, `panic!`, and `assert!` is never touched by this or any family — such a message is read only after the program has already failed, so mutating it would pin exact wording rather than test behavior.

### `collection`

Omits one element from a `vec![…]` literal, sweeping up its separating comma so the remaining list still parses. A surviving mutant means no test is sensitive to that particular element being present — the collection's length or membership at that position is never asserted.

```rust
// original
let allowed = vec!["GET", "POST", "DELETE"];

// collection.omit_element (removing "POST")
let allowed = vec!["GET", "DELETE"];
```

Restricted to `vec!` and never a fixed-size array, because an array's length is part of its type; removing an element from `[T; N]` would change `N` rather than the value.

## Every mutator

The `Alias` column gives the academic name where the mutator has one, so a mutation-testing
paper's terminology selects the same thing this tool calls something else. `Default` records whether
the mutator runs when `--mutators` is not given.

<!-- begin generated: mutators -->

#### `fn_value`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `fn_value.default` | replace the function body with a default value | `RV` | yes |
| `fn_value.stated` | replace the body with the value the site states in #[gamma::value(...)] | `RV` | yes |
| `fn_value.unit` | replace the body of a unit function with () |  | yes |
| `fn_value.bool_true` | replace the body with true |  | yes |
| `fn_value.bool_false` | replace the body with false |  | yes |
| `fn_value.zero` | replace the body with 0 |  | yes |
| `fn_value.one` | replace the body with 1 |  | yes |
| `fn_value.minus_one` | replace the body with -1 |  | yes |
| `fn_value.empty_string` | replace the body with an empty string |  | yes |
| `fn_value.xyzzy_string` | replace the body with a non-empty string |  | yes |
| `fn_value.none` | replace the body with None |  | yes |
| `fn_value.some_default` | replace the body with Some(Default::default()) |  | yes |
| `fn_value.ok_default` | replace the body with Ok(Default::default()) |  | yes |
| `fn_value.err_default` | replace the body with Err(Default::default()) |  | yes |
| `fn_value.err_with` | replace the body with Err(v) for each --error value |  | yes |
| `fn_value.two` | replace the body with 2 |  | yes |
| `fn_value.some` | replace the body with Some(value) |  | no |
| `fn_value.ok` | replace the body with Ok(value) |  | yes |
| `fn_value.empty_collection` | replace the body with an empty collection or iterator |  | yes |
| `fn_value.one_element` | replace the body with a one-element collection or iterator |  | yes |
| `fn_value.tuple` | replace the body with a tuple of replacement values |  | yes |

#### `relational`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `relational.lt_to_le` | replace < with <= | `ROR` | yes |
| `relational.lt_to_gt` | replace < with > | `ROR` | yes |
| `relational.le_to_lt` | replace <= with < | `ROR` | yes |
| `relational.le_to_ge` | replace <= with >= | `ROR` | yes |
| `relational.gt_to_ge` | replace > with >= | `ROR` | yes |
| `relational.gt_to_lt` | replace > with < | `ROR` | yes |
| `relational.ge_to_gt` | replace >= with > | `ROR` | yes |
| `relational.ge_to_le` | replace >= with <= | `ROR` | yes |
| `relational.eq_to_ne` | replace == with != | `ROR` | yes |
| `relational.ne_to_eq` | replace != with == | `ROR` | yes |

#### `arith`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `arith.add_to_sub` | replace + with - | `AOR` | yes |
| `arith.add_to_mul` | replace + with * | `AOR` | yes |
| `arith.sub_to_add` | replace - with + | `AOR` | yes |
| `arith.sub_to_div` | replace - with / | `AOR` | yes |
| `arith.mul_to_div` | replace * with / | `AOR` | yes |
| `arith.mul_to_add` | replace * with + | `AOR` | yes |
| `arith.div_to_mul` | replace / with * | `AOR` | yes |
| `arith.div_to_rem` | replace / with % | `AOR` | yes |
| `arith.rem_to_div` | replace % with / | `AOR` | yes |
| `arith.rem_to_mul` | replace % with * | `AOR` | yes |

#### `bitwise`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `bitwise.and_to_or` | replace & with \| | `AOR` | yes |
| `bitwise.or_to_and` | replace \| with & | `AOR` | yes |
| `bitwise.xor_to_and` | replace ^ with & | `AOR` | yes |
| `bitwise.and_to_xor` | replace & with ^ | `AOR` | yes |

#### `shift`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `shift.shl_to_shr` | replace << with >> | `AOR` | yes |
| `shift.shr_to_shl` | replace >> with << | `AOR` | yes |

#### `assign`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `assign.add_to_sub` | replace += with -= | `ASR` | yes |
| `assign.sub_to_add` | replace -= with += | `ASR` | yes |
| `assign.mul_to_div` | replace *= with /= | `ASR` | yes |
| `assign.div_to_mul` | replace /= with *= | `ASR` | yes |
| `assign.rem_to_div` | replace %= with /= | `ASR` | yes |
| `assign.and_to_or` | replace &= with \|= | `ASR` | yes |
| `assign.or_to_and` | replace \|= with &= | `ASR` | yes |
| `assign.xor_to_and` | replace ^= with &= | `ASR` | yes |
| `assign.shl_to_shr` | replace <<= with >>= | `ASR` | yes |
| `assign.shr_to_shl` | replace >>= with <<= | `ASR` | yes |

#### `logical`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `logical.and_to_or` | replace && with \|\| | `LCR` | yes |
| `logical.or_to_and` | replace \|\| with && | `LCR` | yes |

#### `cond`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `cond.negate` | negate a branch condition | `COR` | yes |
| `cond.always_true` | force a branch condition to true | `COR` | yes |
| `cond.always_false` | force a branch condition to false | `COR` | yes |

#### `match_guard`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `match_guard.negate` | negate a match arm's guard | `COR` | yes |
| `match_guard.always_true` | force a match arm's guard to true | `COR` | yes |
| `match_guard.always_false` | force a match arm's guard to false | `COR` | yes |

#### `match_arm`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `match_arm.never_matches` | stop a match arm from matching, falling through to the wildcard | `SDL` | yes |

#### `struct_field`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `struct_field.omit` | omit a struct literal field, leaving the base expression to supply it | `SDL` | yes |

#### `range`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `range.exclusive_to_inclusive` | extend a .. range to cover its endpoint | `ROR` | yes |
| `range.inclusive_to_exclusive` | shrink a ..= range to stop short of its endpoint | `ROR` | yes |

#### `loop`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `loop.break_to_continue` | replace break with continue |  | yes |
| `loop.continue_to_break` | replace continue with break |  | yes |
| `loop.delete_break` | delete a break statement | `SDL` | yes |
| `loop.delete_continue` | delete a continue statement | `SDL` | yes |

#### `unary`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `unary.remove_neg` | remove a unary minus | `UOI` | yes |
| `unary.remove_not` | remove a unary not | `UOI` | yes |

#### `literal`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `literal.int_to_zero` | replace an integer literal with 0 | `CRP` | yes |
| `literal.int_to_one` | replace an integer literal with 1 | `CRP` | yes |
| `literal.int_increment` | add one to an integer literal | `CRP` | yes |
| `literal.int_decrement` | subtract one from an integer literal | `CRP` | yes |
| `literal.bool_flip` | invert a boolean literal | `CRP` | yes |
| `literal.str_to_empty` | replace a string literal with an empty string | `CRP` | yes |
| `literal.str_to_xyzzy` | replace a string literal with a different string | `CRP` | yes |

#### `stmt`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `stmt.delete_call` | delete a statement whose value is discarded | `SDL` | yes |
| `stmt.delete_assign` | delete an assignment statement, plain or compound | `SDL` | yes |

#### `expr`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `expr.increment` | add one to a numeric expression in a boundary-sensitive position | `EVR` | yes |
| `expr.decrement` | subtract one from a numeric expression in a boundary-sensitive position | `EVR` | yes |

#### `option`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `option.some_to_none` | replace Some(value) with None | `EVR` | yes |
| `option.none_to_some` | replace None with Some(Default::default()) | `EVR` | yes |

#### `result`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `result.ok_to_err` | replace Ok(value) with Err(Default::default()) | `EVR` | yes |
| `result.err_to_ok` | replace Err(value) with Ok(Default::default()) | `EVR` | yes |

#### `iter`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `iter.any_to_all` | replace any with all | `EVR` | yes |
| `iter.all_to_any` | replace all with any | `EVR` | yes |
| `iter.min_to_max` | replace min with max | `EVR` | yes |
| `iter.max_to_min` | replace max with min | `EVR` | yes |
| `iter.first_to_last` | replace first with last | `EVR` | yes |
| `iter.last_to_first` | replace last with first | `EVR` | yes |
| `iter.remove_sort` | remove a sort from a chain | `SDL` | yes |
| `iter.remove_dedup` | remove a deduplication from a chain | `SDL` | yes |

#### `string`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `string.starts_with_to_ends_with` | replace starts_with with ends_with | `EVR` | yes |
| `string.ends_with_to_starts_with` | replace ends_with with starts_with | `EVR` | yes |
| `string.lower_to_upper` | replace to_lowercase with to_uppercase | `EVR` | yes |
| `string.upper_to_lower` | replace to_uppercase with to_lowercase | `EVR` | yes |
| `string.trim_start_to_trim_end` | replace trim_start with trim_end | `EVR` | yes |
| `string.trim_end_to_trim_start` | replace trim_end with trim_start | `EVR` | yes |

#### `collection`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `collection.omit_element` | omit an element from a vec! literal | `SDL` | yes |

#### `assign_value`

| Mutator | What it does | Alias | Default |
| --- | --- | --- | --- |
| `assign_value.default` | replace an assigned value with its type's default | `EVR` | yes |

<!-- end generated -->

## What the catalog deliberately omits

Three constraints shape what the catalog can express, and all three come from one fact: a mutant is
run by wrapping its site as `if guard { mutant } else { original }`, so a mutant must have the same
*type* as the code it replaces.

**The string mutators leave failure messages alone.** The argument to `expect` and `expect_err` is
not touched, and nothing inside a macro is traversed at all, so the text in `panic!`, `assert!` and
`assert_eq!` is exempt on the same grounds. Such a message is read only once the program has already
given up, so rewriting it changes what a crash prints rather than what the program does. Killing one
would mean asserting the exact wording of a panic, which pins phrasing that should stay free to
improve and turns a typo fix into a failing suite. The call itself is still mutated: `expect` can be
renamed, and the value it is asked of is ordinary code.

**`range` moves the endpoint rather than rewriting `..` as `..=`.** The two say the same thing —
`a..b + 1` covers exactly what `a..=b` covers — but `Range` and `RangeInclusive` are different
types, so the literal rewrite could never compile.

**`iter` swaps only methods whose two spellings agree on a type.** `any`/`all` and
`starts_with`/`ends_with` return `bool`; `min`/`max` and `first`/`last` return `Option<T>`. Swapping
`take` for `skip` asks a real question about a chain, but `Take<I>` and `Skip<I>` are different
types, so it is absent — as is dropping a `filter`, which would turn `Filter<I>` back into `I`.
`sort` and `dedup` return `()` and work in place, so they are reached by deleting the statement
instead.

**A function returning `impl Iterator` gets no return-value mutants at all.** An `impl Trait` return
is a single concrete type chosen by the body, so `Empty<T>`, `Once<T>` and whatever the author
actually wrote are three different types that cannot be two arms of one `if`.

**The perturbation mutators only fire where the source says the value is a number.** `+ 1` and
`- 1` need an integer, and without type resolution the only evidence available is what the source
wrote down: an annotation, an initializer that can itself be typed, a struct field, a cast,
arithmetic, a numeric-only method such as `saturating_sub`, or a use that admits nothing else, such
as indexing with the value or comparing it against an integer literal. Where none of that is
present the mutators stay silent rather than guess, avoiding unviable mutants on non-numeric
expressions that fail to build. The cost of the rule is real and runs the other way too: a value that
is a number but never explicitly typed or indexed is a question that goes unasked.

A fourth omission has a different cause, and is worth knowing for a different reason. **Const
contexts generate no mutants at all** — the body of a `const fn`, a `const` or `static`
initializer, an array length, and anything else evaluated at compile time. The guard that selects
between the mutant and the original is a function call, which const evaluation does not allow, so a
mutant there could never compile and would only be withdrawn as noise. The consequence to keep in
mind is about reading a report rather than about the catalog: a file full of `const fn` will show
few mutants or none, and that is a statement about where this tool can reach, not evidence that the
code is well tested.

### How return values are synthesized

`fn_value` recurses through the return type rather than reaching straight for `Default::default()`.
A `Result<Option<bool>, E>` yields `Err(Default::default())`, `Ok(None)`, `Ok(Some(true))` and
`Ok(Some(false))`, and the same recursion covers tuples, collections, maps, `Box`, `Rc`, `Arc`,
`Cow` and `NonZero`. Depth and width are bounded so a deeply generic signature cannot generate an
unbounded population.

Where the tool cannot name a value of a type it falls back to `Default::default()`, optimistically:
a concrete type it has never heard of usually does have a `Default`. It withholds that guess only
where nothing could support it — a bare type parameter, an associated type projected out of one such
as `D::Error`, an `impl Trait` that is not an iterator, or a `Box<dyn Trait>`. A parameter declared
`T: Default` keeps its mutant, because there the promise is explicit. Where you know a value the
signature accepts, [state it](#stating-the-value-yourself) and the mutant comes back.

To reach an error type that has no `Default`, name the values yourself:

```bash
cargo gamma run --error 'MyError::Io' --error 'MyError::Eof'
```

Each becomes its own `fn_value.err_with` mutant on every function returning a `Result`.

That guess is only made when the return type actually spells the error. The widespread crate-local
alias `pub type Result<T> = std::result::Result<T, MyError>` does not, so functions returning it get
their `Ok` mutants and no `Err` one — the alias fixed the error to something the tool cannot see,
and it is rarely a type with a `Default`. Use `--error` to supply values for it.

Types are recognized by the last segment of their path, because a standard type may be written
bare, fully qualified, or re-exported and there is no name resolution here to tell those apart. To
stop that taking every type ending in `Vec` for the standard one, the type arguments are counted as
well: your own `Vec` carrying none is not the standard `Vec`, which carries an element type, so it
falls back to `Default::default()` instead of being handed `Vec::new()`. A type that shadows a
standard name *and* matches its shape is genuinely indistinguishable and will produce a mutant that
does not compile, reported as unviable.

A function returning a reference is served by leaking a box, so `fn name(&self) -> &String` offers
`&*Box::leak(Box::new(String::new()))` and the other values `String` would offer. The plain spelling
would borrow a temporary that dies at the end of the expression and so would never compile; leaking
yields a reference that outlives the call. `Box::leak` hands back `&mut T`, which is reborrowed when
the signature asked for `&T` — coercion would cover a return position, but not one where the value
is what a type is inferred from. The leak lasts as long as the test process, which exits shortly
afterwards, though a mutant leaking on a hot path can reach the memory limit and be reported as
`OUTOFMEM`. A reference to a trait object is still passed over, because there is no value of it to
make.

A function returning `impl Iterator` is offered `core::iter::empty()`, and `core::iter::once(v)` for
each value its `Item` type yields. This is the one return type whose mutant cannot simply be dropped
into the guard: `impl Iterator` is a single concrete type chosen by the body, so the two arms of
`if a(n) { core::iter::empty() } else { ..the body.. }` would disagree and nothing would compile.
Both arms are wrapped in a variant of `gamma_rt::Either`, a two-parameter enum that is an iterator
whenever both of its sides are. Nothing is allocated, nothing is dynamically dispatched, and
`Send`, `Sync`, `Clone` and the exact-size and double-ended bounds all survive, because the compiler
derives them from the two sides — which a `Box<dyn Iterator>` would have erased, breaking every
signature that promised one of them. `impl ExactSizeIterator`, `impl DoubleEndedIterator` and
`impl FusedIterator` are covered on the same footing. A bare `impl Iterator` that never writes
`Item` still gets the empty mutant, since that one needs no item type — the wrapper infers it from
the arm holding the original.

### Stating the value yourself

Everything above is a guess made from the text of a return type, and there are signatures no guess
can be made from. `#[gamma::value(<expr>)]` lets the code say what the mutant should substitute:

```rust
#[gamma::value(Box::new(NullSink))]
pub fn sink(&self) -> Box<dyn Sink> {
    Box::new(FileSink::new(&self.path))
}
```

The site receives a `fn_value.stated` mutant substituting `Box::new(NullSink)` instead of being
omitted. The attribute comes from the `cargo-gamma-attrs` crate, the same dependency the
[suppression attributes](../README.md#suppressing-mutations) come from, and it is inert: it expands
to the item unchanged, so it costs a normal build nothing.

It is worth reaching for in two situations:

- **A site the tool withholds a mutant from.** A bare type parameter, an associated type, a
  `Box<dyn Trait>` or a non-iterator `impl Trait` are all types with no value the tool can name.
  Stating one creates the mutant, and the function stops being invisible to the family whose whole
  question is whether anything checks what it returns.
- **A site whose guess is not the interesting wrong answer.** A stated value replaces the guessed
  ones at that site rather than joining them, so `#[gamma::value(u32::MAX)]` on a function the tool
  would have handed `0` asks the question you meant to ask instead of one more you did not.

Behind an alias or an unknown concrete type the guess is `Default::default()`, which is a hope that
the type implements `Default`; stating a value is how that hope becomes a fact.

The rules are deliberately few:

- **One expression, one site.** The argument is a single Rust expression. Nothing, two of them, or
  something that is not an expression is a compile error, and so is a second `#[gamma::value]` on
  one item — last-wins would make the mutant depend on the order two attributes were written in.
- **On a function or a method only.** It is rejected on an `impl` block, a module or anything else,
  and it is never inherited from an enclosing item: one expression that type-checks as the body of
  every function beneath a module essentially never exists, so the alternative to rejecting it is
  silently applying it where it cannot hold. A trait method that is only declared is rejected too —
  there is no body to replace, and the implementations do not inherit it either.
- **It can only add or replace, never remove.** There is no spelling of it that deletes a site or
  touches another family. Suppression is a separate channel, with its own vocabulary and its own
  reason strings, and it stays the only thing a reviewer has to read to see what was excluded. A
  site may carry both, in which case the suppression wins.
- **Nothing is taken on trust.** The tool does not type-check, so it cannot tell a value the
  signature accepts from one it does not, and it does not try. A wrong one becomes an ordinary
  mutant that fails to build and is withdrawn as `unviable`, exactly like a wrong guess. Refusing to
  generate it would mean guessing again, in the direction of silently discarding the mutants of
  authors who were right.
- **Not on a `const fn` or an empty body.** Stated-value mutation is supported only on non-const
  functions and methods with non-empty bodies.

A supported stated value is visible without building anything in `cargo gamma list mutants`.
Unsupported annotations are rejected rather than treated as working hints that quietly do nothing.

### Mutants that are never offered

Some mutants are withheld because they could not tell you anything, however good your tests are.

A replacement that reproduces the code it replaces is not offered at all. `fn ready() -> bool
{ true }` is not given `fn_value.bool_true`, because the mutated program would be identical, no test
could distinguish it, and it would be reported forever as a survivor you cannot kill. The comparison
is made on tokens, so layout and comments do not affect it. The other values are still offered.

`Default::default()` is not offered for the `default` method of an `impl Default`, because there it
names the very function being replaced and the mutant is unbounded recursion rather than a different
answer. The rest of that method is mutated normally — the `7` in `fn default() -> Self { Thing
{ n: 7 } }` is still perturbed — so implementing `Default` does not exempt a type from measurement.

### Mutants that cannot compile

Because everything is on, some mutants will not compile — `struct_field.omit` fires on every literal
struct, and `expr` perturbs values it cannot always prove are numeric. These are withdrawn
automatically, in batches rather than one build each, and reported as `unviable` rather than counted
against the score. `--show-unviable` lists them if you want to see what was discarded.

## Mutator presets

A mutator preset is a named set of mutators. It exists so that a question you ask often — *"is my
arithmetic tested?"*, *"what would cargo-mutants have found?"* — is one word on the command line
rather than a list you have to keep in your head and keep up to date as the catalog grows.

Presets are written with a leading `@` and are accepted anywhere a mutator name is: `--mutators`, the
`mutators` key in `gamma.toml`, and every [suppression](../README.md#suppressing-mutations) directive. They compose with
families, individual mutators and `!` negation, applied left to right:

```bash
cargo gamma run --mutators @arithmetic            # just the number crunching
cargo gamma run --mutators @control,@logical      # two presets at once
cargo gamma run --mutators @all,!stmt             # everything but statement deletion
cargo gamma run --mutators @numeric,!literal.int_increment  # a preset, less one mutator
```

`cargo gamma list presets` prints the table below resolved against your current configuration.

<!-- begin generated: presets -->

| Mutator preset | What it selects | Expands to |
| --- | --- | --- |
| `@all` | every registered mutator | `*` |
| `@default` | the mutators enabled when none are named | `@default` |
| `@pedantic` | additional low-yield mutations excluded from the default selection | `fn_value.some` |
| `@boundary` | relational and boundary conditions | `relational`, `range` |
| `@arithmetic` | arithmetic, bitwise, shift and compound assignment | `arith`, `bitwise`, `shift`, `assign` |
| `@logical` | logical operators and branch conditions | `logical`, `cond`, `match_guard` |
| `@control` | the choices control flow makes: conditions, guards, arms and loop exits | `cond`, `match_guard`, `match_arm`, `loop` |
| `@removal` | statement and side-effect deletion | `stmt`, `unary`, `match_arm`, `struct_field`, `collection` |
| `@semantics` | standard-library meaning: Option, Result, iterators, strings and collections | `option`, `result`, `iter`, `string`, `collection`, `assign_value` |
| `@literals` | literal and constant replacement | `literal` |
| `@numeric` | literal replacement and focused numeric expression perturbation | `literal`, `expr` |
| `@extreme` | a synonym for `all`, kept because scripts name it | `*` |

<!-- end generated -->

### Which one to reach for

**`@default` is what you get with no `--mutators` at all.** It is named
so that a script can say what it means, and so that `@default,!literal` reads as an adjustment to
the shipped policy rather than a list that has to be re-derived every release.

**`@pedantic` currently contains only `fn_value.some`.** Select it alone to study that mutation, or
use `--mutators @default,@pedantic` to add it to an ordinary run. Membership is deliberately narrow
until cross-repository evidence supports more candidates.

**`@extreme` is a second spelling of `@all`.** It selects the entire catalog as an alias for `@all`.

**`@boundary` is the highest-yield preset per mutant.** Off-by-one errors are the defect class
mutation testing is best at exposing, and a surviving `relational` or `range` mutant almost always
names a real missing assertion rather than an equivalent program.

A preset is a starting point, not a commitment. If a preset is close but not right, name it and
subtract: `--mutators @semantics,!option` keeps the shape of the preset and records the one deviation in
a form a reviewer can read.

**There is deliberately no cargo-mutants parity preset.** The families that tool also mutates cut
across several presets, and a single name for them would suggest an equivalence the two catalogs
do not have. [Comparing the numbers](../README.md#comparing-the-numbers) gives the explicit
selector list and says where the populations still diverge.
