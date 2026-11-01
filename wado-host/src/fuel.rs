//! What fuel a guest spends: each Wasm operator is priced near the time it
//! takes, so fuel tracks run time while staying deterministic, and a fuel
//! budget bounds time.
//!
//! The classes follow the Internet Computer's instruction weights
//! (`instruction_to_cost` in dfinity/ic), which were benchmarked per operator.
//! What that table leaves unpriced or prices for another engine — GC
//! allocation, `call_ref`, casts, bulk memory — is priced from microbenchmarks
//! of each operator against `i32.add` under wasmtime 50's Cranelift with the
//! copying collector.
//!
//! A price is a `u8`, and wasmtime charges a bulk operator a flat price plus a
//! whole number per element, so the table is an approximation.
//!
//! Figures do not compare across a change of price, so a change here renames
//! the fuel suite the Benchmark workflow records (`name:` in
//! `.github/workflows/benchmark.yml`).

use wasmtime::{Config, OperatorCost};

/// The fuel of `i32.add`. Two rather than one so that what costs half of it —
/// a local or a constant, which Cranelift keeps in a register — has a price.
const UNIT: u8 = 2;
const HALF_UNIT: u8 = 1;

const fn units(n: u8) -> u8 {
    n * UNIT
}

/// Turn on fuel consumption in `config`, priced by [`operator_cost`].
pub fn meter_fuel(config: &mut Config) {
    config.consume_fuel(true);
    config.operator_cost(operator_cost());
}

macro_rules! scale_wasmtime_default {
    ($( @$proposal:ident $op:ident $({ $($arg:ident: $argty:ty),* })? => $visit:ident ($($ann:tt)*) )*) => {
        /// wasmtime prices each operator at zero (no code) or one; one becomes
        /// [`UNIT`].
        fn scale_wasmtime_default(cost: &mut OperatorCost) {
            $( cost.$op *= UNIT; )*
        }
    };
}
wasmparser::for_each_operator!(scale_wasmtime_default);

macro_rules! price {
    ($cost:ident, $fuel:expr; $($op:ident),+ $(,)?) => {
        $( $cost.$op = $fuel; )+
    };
}

fn operator_cost() -> OperatorCost {
    let mut c = OperatorCost::new();
    scale_wasmtime_default(&mut c);

    price!(c, HALF_UNIT; LocalGet, LocalSet, LocalTee, I32Const, I64Const, F32Const, F64Const, RefNull);
    price!(c, units(2);
        F32Add, F32Sub, F32Mul, F64Add, F64Sub, F64Mul, F32Copysign,
        F32Ceil, F32Floor, F32Trunc, F32Nearest, F64Ceil, F64Floor, F64Trunc, F64Nearest,
        If, Br, BrIf, BrTable, GlobalGet, GlobalSet,
        ArrayGet, ArrayGetS, ArrayGetU, ArraySet,
    );
    price!(c, units(2) + HALF_UNIT;
        RefTestNonNull, RefTestNullable, RefCastNonNull, RefCastNullable, BrOnCast, BrOnCastFail,
    );
    price!(c, units(3);
        F32Div, F64Copysign,
        F32Eq, F32Ne, F32Lt, F32Gt, F32Le, F32Ge, F64Eq, F64Ne, F64Lt, F64Gt, F64Le, F64Ge,
        F32ConvertI32S, F32ConvertI64S, F64ConvertI32S, F64ConvertI64S,
        ReturnCall, ReturnCallRef,
    );
    price!(c, units(5); F64Div, F32Sqrt, TableGet, TableSet, RefIsNull, Call, CallRef);
    price!(c, units(8); F64Sqrt);
    price!(c, units(10);
        I32DivS, I32DivU, I32RemS, I32RemU, I64DivS, I64DivU, I64RemS, I64RemU, CallIndirect,
    );
    price!(c, units(16); F32ConvertI32U, F32ConvertI64U, F64ConvertI32U, F64ConvertI64U);
    price!(c, units(18); F32Min, F32Max, F64Min, F64Max);
    price!(c, units(20);
        I32TruncF32S, I32TruncF32U, I32TruncF64S, I32TruncF64U,
        I64TruncF32S, I64TruncF32U, I64TruncF64S, I64TruncF64U,
        I32TruncSatF32S, I32TruncSatF32U, I32TruncSatF64S, I32TruncSatF64U,
        I64TruncSatF32S, I64TruncSatF32U, I64TruncSatF64S, I64TruncSatF64U,
        MemorySize,
    );
    price!(c, units(60); ReturnCallIndirect);
    price!(c, units(100); TableSize, TableFill, TableCopy, TableInit);
    // 130 and 300 units, past what a `u8` holds at this unit.
    price!(c, u8::MAX; RefFunc, MemoryGrow, TableGrow, DataDrop, ElemDrop);

    // An allocation is an inline bump plus its share of the collections it
    // causes. That share grows with the live set; these prices assume a small
    // one.
    price!(c, units(16); StructNew, StructNewDefault);
    price!(c, units(18); ArrayNew, ArrayNewDefault, ArrayNewFixed, ArrayNewData, ArrayNewElem);
    price!(c, units(12); ArrayFill, ArrayInitData, ArrayInitElem);
    price!(c, units(13); ArrayCopy);
    // A libcall into `memmove` / `memset`. Its per-byte time is under a
    // hundredth of a unit, so it charges nothing per byte.
    price!(c, units(14); MemoryCopy, MemoryFill, MemoryInit);

    let v = &mut c.variable;
    // wasmtime fills an array one element at a time.
    v.array_new_per_element = UNIT;
    v.array_new_default_per_element = UNIT;
    v.array_new_elem_per_element = UNIT;
    v.array_fill_per_element = UNIT;
    v.array_init_elem_per_element = UNIT;
    v.array_new_data_per_element = HALF_UNIT;
    v.array_init_data_per_element = HALF_UNIT;
    // A copy is a `memmove`; growth and table operations are priced flat.
    v.array_copy_per_element = 0;
    v.memory_copy_per_byte = 0;
    v.memory_fill_per_byte = 0;
    v.memory_init_per_byte = 0;
    v.memory_grow_per_page = 0;
    v.table_copy_per_element = 0;
    v.table_fill_per_element = 0;
    v.table_init_per_element = 0;
    v.table_grow_per_element = 0;
    c
}

#[cfg(test)]
mod tests {
    use wasmtime::{Engine, Instance, Module, OptLevel, Store};

    use super::*;
    use crate::engine::default_config;

    /// The fuel `export` spends on one call with `arg`.
    fn fuel_of(wat: &str, export: &str, arg: i32) -> u64 {
        let mut config = default_config(OptLevel::None);
        meter_fuel(&mut config);
        let engine = Engine::new(&config).unwrap();
        let module = Module::new(&engine, wat).unwrap();
        let mut store = Store::new(&engine, ());
        store.set_fuel(u64::MAX).unwrap();
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let func = instance
            .get_typed_func::<i32, i32>(&mut store, export)
            .unwrap();
        let before = store.get_fuel().unwrap();
        func.call(&mut store, arg).unwrap();
        before - store.get_fuel().unwrap()
    }

    const MODULE: &str = r#"(module
        (type $words (array (mut i32)))
        (func (export "add") (param i32) (result i32)
            (i32.add (local.get 0) (local.get 0)))
        (func (export "div") (param i32) (result i32)
            (i32.div_u (local.get 0) (local.get 0)))
        (func (export "array") (param i32) (result i32)
            (array.len (array.new_default $words (local.get 0)))))"#;

    #[test]
    fn integer_division_costs_ten_additions() {
        let add = fuel_of(MODULE, "add", 1);
        let div = fuel_of(MODULE, "div", 1);
        assert_eq!(div - add, 9 * u64::from(UNIT));
    }

    #[test]
    fn array_new_default_charges_each_element() {
        let empty = fuel_of(MODULE, "array", 0);
        let hundred = fuel_of(MODULE, "array", 100);
        assert_eq!(hundred - empty, 100 * u64::from(UNIT));
    }
}
