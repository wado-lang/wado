//! Unified peephole engine pass.
//!
//! Runs the position-flexible local peephole rewrite rules — short `push_str`
//! simplification (`string_push`), array-literal materialization
//! (`array_literal`), and write-only local elimination (`elide_local`) —
//! together over one shared worklist per function. The engine session (parent
//! map, use index, post-order seed) is built once for the three rules instead
//! of once per rule, and the rules interleave on a single worklist rather than
//! running as three independent whole-body sweeps.
//!
//! This is the first consolidation step of the worklist rewrite engine: the
//! three rules already ran on the engine, but each rebuilt its own session and
//! ran in isolation. See `docs/wep-2026-06-05-nir-rewrite-engine-design.md`.
//!
//! Two rules keep their own standalone engine passes:
//!
//! - `match_to_switch` must run before every other in-loop pass (so they see
//!   the `Switch` shape) and also runs at `-O0`, where the loop is skipped.
//! - `select_lowering` is a terminal post-loop lowering (`If` → `select`) that
//!   must run after all other transformations.
//!
//! The pass is invoked at two points in the fixed-point loop — after value-copy
//! elision / demotion (so `string_push` sees the stripped receiver) and after
//! `inline` (so `array_literal` sees the exposed `array_new + push` window).
//! `array_literal` no-ops in the first run and `string_push` no-ops in the
//! second; both bail immediately on a non-matching node, so the wasted dispatch
//! is negligible. `elide_local` runs in both, which only widens its reach.

use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;

use super::array_literal::{Collapser, resolve_array_push_names};
use super::elide_local::ElideRule;
use super::string_push::{ShortPushStrRule, resolve_ctx};

/// Run the unified peephole rule set over every function body. Returns whether
/// any rule fired.
pub(super) fn run_peephole(project: &mut NirPackage) -> bool {
    // Whole-package contexts, resolved once before the mutable body walk.
    let push_names = resolve_array_push_names(project);
    let array_rule = Collapser::new(&push_names);
    let push_rule = resolve_ctx(project).map(ShortPushStrRule::new);

    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        // `stores_aliased_locals` is per-function, so the elide rule is rebuilt
        // for each body; the borrow is dropped before the body is taken.
        let stores_aliased = func.stores_aliased_locals.clone();
        let elide_rule = ElideRule::new(&stores_aliased);
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let mut rules: Vec<&dyn Rule> = vec![&array_rule, &elide_rule];
        if let Some(push_rule) = push_rule.as_ref() {
            rules.push(push_rule);
        }
        let mut engine = Engine::new(body);
        changed |= engine.run(&rules);
    }
    changed
}
