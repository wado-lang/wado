//! Generic visitor traits for mutable and immutable traversal of WIR instruction trees.
//!
//! These are used by WIR optimization passes that need to walk instruction trees.
//! The visitors handle control-flow body traversal (Block, Loop, If, Seq) in one
//! place so that individual passes don't need to duplicate that logic.

use crate::wir::WirInstr;

/// Trait for immutable traversal of WIR instruction trees.
///
/// Override `visit_instr` to add custom logic at each node.
/// Call `self.walk_instr(instr)` within your override to recurse into children.
/// Override `visit_body` to add custom logic when entering an instruction body.
pub trait WirRefVisitor {
    fn visit_instr(&mut self, instr: &WirInstr) {
        self.walk_instr(instr);
    }

    fn visit_body(&mut self, body: &[WirInstr]) {
        self.walk_body(body);
    }

    fn walk_body(&mut self, body: &[WirInstr]) {
        for instr in body {
            self.visit_instr(instr);
        }
    }

    fn walk_instr(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::Block { body, .. }
            | WirInstr::Loop { body, .. }
            | WirInstr::Seq(body) => {
                self.visit_body(body);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_instr(condition);
                self.visit_body(then_body);
                if let Some(eb) = else_body {
                    self.visit_body(eb);
                }
            }
            _ => {
                instr.for_each_child(&mut |child| self.visit_instr(child));
            }
        }
    }
}

/// Trait for mutable traversal of WIR instruction trees.
///
/// Override `visit_instr` to add custom logic at each node.
/// Call `self.walk_instr(instr)` within your override to recurse into children.
/// Override `visit_body` to add custom logic when entering an instruction body
/// (e.g., removing nops, truncating dead code).
pub trait WirMutVisitor {
    fn visit_instr(&mut self, instr: &mut WirInstr) {
        self.walk_instr(instr);
    }

    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
    }

    fn walk_body(&mut self, body: &mut Vec<WirInstr>) {
        for instr in body.iter_mut() {
            self.visit_instr(instr);
        }
    }

    fn walk_instr(&mut self, instr: &mut WirInstr) {
        match instr {
            WirInstr::Block { body, .. }
            | WirInstr::Loop { body, .. }
            | WirInstr::Seq(body) => {
                self.visit_body(body);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.visit_instr(condition);
                self.visit_body(then_body);
                if let Some(eb) = else_body {
                    self.visit_body(eb);
                }
            }
            other => {
                other.for_each_boxed_child_mut(&mut |child| self.visit_instr(child));
            }
        }
    }
}
