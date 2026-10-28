// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use quote::ToTokens as _;
use syn::{Block, Expr, Stmt};

use crate::HashMap;

/// Describes the path reaching one edit, not unconditional facts about every loop execution.
///
/// Unchanged guards and surrounding statements preserve that path's meaning; earlier exiting
/// branches must not be taken. The interpreter must still check feasibility and replay the
/// resulting assumptions to establish that the same path remains stable on recurrence.
pub(super) fn conditions(original: &Block, mutated: &Block) -> HashMap<String, bool> {
    let mut result = HashMap::default();
    if focus(original, mutated, &mut result, &mut 64).is_none() {
        result.clear();
    }
    result
}

fn focus(original: &Block, mutated: &Block, result: &mut HashMap<String, bool>, remaining: &mut usize) -> Option<()> {
    *remaining = remaining.checked_sub(1)?;
    let prefix = original
        .stmts
        .iter()
        .zip(&mutated.stmts)
        .take_while(|(left, right)| left == right)
        .count();
    for statement in &mutated.stmts[..prefix] {
        *remaining = remaining.checked_sub(1)?;
        if let Stmt::Expr(Expr::If(branch), _) = statement
            && branch.attrs.is_empty()
            && branch.else_branch.is_none()
            && exits(&branch.then_branch)
        {
            let _old = result.insert(branch.cond.to_token_stream().to_string(), false);
        }
    }
    let (Some(Stmt::Expr(Expr::If(left), _)), Some(Stmt::Expr(Expr::If(right), _))) =
        (original.stmts.get(prefix), mutated.stmts.get(prefix))
    else {
        return Some(());
    };
    if !left.attrs.is_empty()
        || !right.attrs.is_empty()
        || left.cond != right.cond
        || original.stmts[prefix + 1..] != mutated.stmts[prefix + 1..]
    {
        return Some(());
    }
    let condition = right.cond.to_token_stream().to_string();
    if left.then_branch != right.then_branch && left.else_branch == right.else_branch {
        let _old = result.insert(right.cond.to_token_stream().to_string(), true);
        focus(&left.then_branch, &right.then_branch, result, remaining)?;
    } else if left.then_branch == right.then_branch
        && let (Some((_, left)), Some((_, right))) = (&left.else_branch, &right.else_branch)
        && let (Expr::Block(left), Expr::Block(right)) = (&**left, &**right)
        && left.attrs.is_empty()
        && right.attrs.is_empty()
        && left.label.is_none()
        && right.label.is_none()
    {
        let _old = result.insert(condition, false);
        focus(&left.block, &right.block, result, remaining)?;
    }
    Some(())
}

fn exits(block: &Block) -> bool {
    matches!(block.stmts.last(),
        Some(Stmt::Expr(Expr::Continue(node), _)) if node.attrs.is_empty() && node.label.is_none())
        || matches!(block.stmts.last(),
            Some(Stmt::Expr(Expr::Break(node), _)) if node.attrs.is_empty() && node.label.is_none() && node.expr.is_none())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::{parse_quote, parse_str};

    use super::*;

    #[test]
    fn exact_path_recursion_exhaustion_discards_all_previously_collected_facts() {
        for (depth, expected) in [(63, HashMap::from_iter([("ready".into(), true)])), (64, HashMap::default())] {
            let wrap = |tail: &str| format!("{{ {} {tail} {} }}", "if ready {".repeat(depth), "}".repeat(depth));
            let original = parse_str(&wrap("cursor += 1;")).unwrap();
            let mutated = parse_str(&wrap("cursor *= 1;")).unwrap();
            assert_eq!(conditions(&original, &mutated), expected, "{depth} nested branches");
        }
    }

    #[test]
    fn changing_only_an_if_statement_semicolon_cannot_invent_a_taken_branch_fact() {
        let original = parse_quote!({
            if ready {};
        });
        let mutated = parse_quote!({ if ready {} });
        assert_eq!(conditions(&original, &mutated), HashMap::default());
    }

    #[test]
    fn prior_exit_facts_survive_an_unfocused_later_edit() {
        let original = parse_quote!({
            if stop {
                continue;
            }
            if ready {
                cursor += 1;
            }
        });
        for mutant in [
            parse_quote!({
                if stop {
                    continue;
                }
                if other {
                    cursor *= 1;
                }
            }),
            parse_quote!({
                if stop {
                    continue;
                }
                #[custom]
                if ready {
                    cursor *= 1;
                }
            }),
        ] {
            assert_eq!(conditions(&original, &mutant), HashMap::from_iter([("stop".into(), false)]));
        }
        let original = parse_quote!({
            if stop {
                continue;
            }
            #[custom]
            if ready {
                cursor += 1;
            }
        });
        let mutant = parse_quote!({
            if stop {
                continue;
            }
            if ready {
                cursor *= 1;
            }
        });
        assert_eq!(conditions(&original, &mutant), HashMap::from_iter([("stop".into(), false)]));
    }

    #[test]
    fn focused_path_budget_discards_partial_facts_at_every_recursion_boundary() {
        for (prefix, accepted) in [(62, true), (63, true), (64, false)] {
            let prefix = format!("if stop {{ continue; }} {}", "();".repeat(prefix - 1));
            let original = parse_str(&format!("{{ {prefix} cursor += 1; }}")).unwrap();
            let mutant = parse_str(&format!("{{ {prefix} cursor *= 1; }}")).unwrap();
            assert_eq!(
                conditions(&original, &mutant),
                if accepted {
                    HashMap::from_iter([("stop".into(), false)])
                } else {
                    HashMap::default()
                }
            );
        }
        for otherwise in [false, true] {
            for (prefix, accepted) in [(62, true), (63, false)] {
                let prefix = format!("if stop {{ continue; }} {}", "();".repeat(prefix - 1));
                let wrap = |body: &str| {
                    if otherwise {
                        format!("{{ if ready {{}} else {{ {body} }} }}")
                    } else {
                        format!("{{ if ready {{ {body} }} }}")
                    }
                };
                let original = parse_str(&wrap(&format!("{prefix} cursor += 1;"))).unwrap();
                let mutant = parse_str(&wrap(&format!("{prefix} cursor *= 1;"))).unwrap();
                assert_eq!(
                    conditions(&original, &mutant),
                    if accepted {
                        HashMap::from_iter([("ready".into(), !otherwise), ("stop".into(), false)])
                    } else {
                        HashMap::default()
                    },
                    "{prefix}; else branch {otherwise}"
                );
            }
        }
    }

    #[test]
    fn focused_paths_include_prior_exits_and_nested_then_else_decisions() {
        let original = parse_quote!({
            if stop {
                continue;
            }
            if ready {
                if selected {
                    cursor += 1;
                } else {
                    cursor += 2;
                }
            } else {
                cursor += 3;
            }
        });
        let mutated = parse_quote!({
            if stop {
                continue;
            }
            if ready {
                if selected {
                    cursor += 1;
                } else {
                    cursor *= 2;
                }
            } else {
                cursor += 3;
            }
        });
        assert_eq!(
            conditions(&original, &mutated),
            HashMap::from_iter([("stop".into(), false), ("ready".into(), true), ("selected".into(), false),])
        );
        let original = parse_quote!({
            if stop {
                break;
            }
            if ready {
                cursor += 1;
            } else {
                if selected {
                    cursor += 2;
                }
            }
        });
        let mutated = parse_quote!({
            if stop {
                break;
            }
            if ready {
                cursor += 1;
            } else {
                if selected {
                    cursor *= 2;
                }
            }
        });
        assert_eq!(
            conditions(&original, &mutated),
            HashMap::from_iter([("stop".into(), false), ("ready".into(), false), ("selected".into(), true),])
        );
    }

    #[test]
    fn path_facts_require_unconditional_exit_and_a_single_unchanged_guard() {
        let originals: [Block; 5] = [
            parse_quote!({
                if stop {
                    work();
                }
                cursor += 1;
            }),
            parse_quote!({
                if stop {
                    continue 'outer;
                }
                cursor += 1;
            }),
            parse_quote!({
                if stop {
                    break 1;
                }
                cursor += 1;
            }),
            parse_quote!({
                if stop {
                    break;
                } else {
                    work();
                }
                cursor += 1;
            }),
            parse_quote!({
                #[cfg(test)]
                if stop {
                    continue;
                }
                cursor += 1;
            }),
        ];
        for original in originals {
            let mut mutant: Block = original.clone();
            mutant.stmts[1] = parse_quote!(cursor *= 1;);
            assert!(conditions(&original, &mutant).is_empty());
        }
        let original = parse_quote!({
            if ready {
                cursor += 1;
            } else {
                cursor += 2;
            }
        });
        for mutant in [
            parse_quote!({
                if other {
                    cursor *= 1;
                } else {
                    cursor += 2;
                }
            }),
            parse_quote!({
                if ready {
                    cursor *= 1;
                } else {
                    cursor *= 2;
                }
            }),
            parse_quote!({
                #[cfg(test)]
                if ready {
                    cursor *= 1;
                } else {
                    cursor += 2;
                }
            }),
            parse_quote!({
                if ready {
                    cursor *= 1;
                } else {
                    cursor += 2;
                }
                side_effect();
            }),
        ] {
            assert!(conditions(&original, &mutant).is_empty());
        }
    }
}
