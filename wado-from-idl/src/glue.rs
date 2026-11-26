//! The JavaScript glue serving a `WebIDL` module's imports from the browser's own
//! objects, generated from the same IR as the Wado module.
//!
//! A web import is a core import of the main module, so each function here
//! takes the import's core signature: the canonical ABI's flat form, except
//! that a handle, an `Option` of one and a closure are each one `externref`.
//! A handle in linear memory is the index of a slot the guest keeps it in,
//! which the glue reads and fills through two exports of the main module.

use std::fmt::Write;

use wado_compiler::component_model::flat_params_in_buffer;
use wado_compiler::name::{
    ExternHostQuery, ExternSlotAccess, callback_export_name, cm_callback_func_name, cm_import_alias,
};

use crate::ir::{DictionaryField, UnionCase, WadoFunction, WadoParam, WadoType};
use crate::webidl::{JsFunction, JsMember, WebIdlOutput};

/// What every glue instance holds besides its imports. The `$` prefix keeps
/// these names apart from every parameter, which is `p<n>`.
const RUNTIME: &str = r#"
// One glue per instance of the component: it holds that instance's memory.
export function $glue() {
  // The main module's memory, its allocator and its exports, which the loader
  // hands over once the module is instantiated.
  let $core;
  const $decoder = new TextDecoder();
  const $encoder = new TextEncoder();
  const $view = () => new DataView($core.memory.buffer);
  const $string = (ptr, len) => $decoder.decode(new Uint8Array($core.memory.buffer, ptr, len));
  // Writes `value` where the guest frees it, and its address and length at `at`.
  const $putString = (at, value) => {
    const bytes = $encoder.encode(value);
    const ptr = $core.realloc(0, 0, 1, bytes.length);
    new Uint8Array($core.memory.buffer, ptr, bytes.length).set(bytes);
    const view = $view();
    view.setUint32(at, ptr, true);
    view.setUint32(at + 4, bytes.length, true);
  };
  const $list = (ptr, len, size, load) =>
    Array.from({ length: len }, (_, i) => load(ptr + i * size));
  // Writes `items` where the guest frees them, and their address and count at
  // `at`.
  const $putList = (at, items, size, align, store) => {
    const list = Array.from(items);
    const ptr = $core.realloc(0, 0, align, list.length * size);
    list.forEach((item, i) => store(ptr + i * size, item));
    const view = $view();
    view.setUint32(at, ptr, true);
    view.setUint32(at + 4, list.length, true);
  };

  // The float whose bits a core integer holds, where a variant's cases share
  // one core value.
  const $bits = new DataView(new ArrayBuffer(8));
  const $f32 = (bits) => {
    $bits.setInt32(0, bits, true);
    return $bits.getFloat32(0, true);
  };
  const $f64 = (bits) => {
    $bits.setBigInt64(0, bits, true);
    return $bits.getFloat64(0, true);
  };

  // The index of the host's `value` among an enum's `values`.
  const $case = (values, value) => {
    const index = values.indexOf(value);
    if (index < 0) {
      throw new TypeError(`"${value}" is a value this package declares no case for`);
    }
    return index;
  };

  // Each declared interface's prototype, to its class.
  let $classes;
  // The class of the nearest ancestor of `object`'s interface that is declared.
  const $classOf = (object) => {
    if ($classes === undefined) {
      $classes = new Map();
      for (const [name, cls] of $CLASSES) {
        if (globalThis[name] !== undefined) {
          $classes.set(globalThis[name].prototype, cls);
        }
      }
    }
    for (let p = Object.getPrototypeOf(object); p !== null; p = Object.getPrototypeOf(p)) {
      const cls = $classes.get(p);
      if (cls !== undefined) {
        return cls;
      }
    }
    throw new TypeError(`${object} is an instance of no interface this package declares`);
  };

  // What an `async` import answers: a promise of the function writing a
  // `Result` at an address, its payload `offset` in. The guest hands the
  // address over where it waits. A reason is any value, a null included,
  // which no handle is.
  const $settle = (promise, offset, ok) =>
    Promise.resolve(promise).then(
      (value) => (at) => {
        $view().setUint8(at, 0);
        ok(at + offset, value);
      },
      (reason) => (at) => {
        const view = $view();
        view.setUint8(at, 1);
        view.setUint32(at + offset, $core.exports[$SLOT_PUT](reason ?? new Error(String(reason))), true);
      },
    );

  // Each export a callback enters by, wrapped so that a `wait` inside it
  // suspends the callback and not the page.
  const $entries = new Map();
  const $enter = (name) => {
    let entry = $entries.get(name);
    if (entry === undefined) {
      entry = WebAssembly.promising($core.exports[name]);
      $entries.set(name, entry);
    }
    return entry;
  };

  // One function per closure, so `removeEventListener` is handed the very
  // function `addEventListener` was. The map holds neither alive.
  const $callbacks = new WeakMap();
  const $callback = (closure, call) => {
    let f = $callbacks.get(closure);
    if (f === undefined) {
      f = (...args) => call(closure, ...args);
      $callbacks.set(closure, f);
    }
    return f;
  };

  return {
    connect(core) {
      $core = core;
    },
    imports: {
"#;

/// The glue module for `output`, naming `source` in its header.
#[must_use]
pub fn generate(output: &WebIdlOutput, source: &str) -> String {
    let module = &output.module;
    let mut out = format!(
        "// Generated by wado-from-idl from {source}. Do not edit.\n\
         // The `{}` module's imports, answered by the browser's own objects.\n\n\
         const $CLASSES = [\n",
        module.package_name
    );
    // A resource without a class is no interface of the host's.
    for resource in &module.resources {
        if let Some(classes) = resource.classes {
            let interface = &output.interfaces[&resource.name];
            writeln!(out, "  [{interface:?}, {}],", classes.lo).unwrap();
        }
    }
    // Each enum's values, at the index its case crosses as.
    out.push_str("];\nconst $ENUMS = {\n");
    for (name, values) in &output.enums {
        writeln!(out, "  {name}: {values:?},").unwrap();
    }
    writeln!(
        out,
        "}};\nconst $SLOT_GET = {:?};\nconst $SLOT_PUT = {:?};",
        ExternSlotAccess::Get.export_name(),
        ExternSlotAccess::Put.export_name()
    )
    .unwrap();
    out.push_str(RUNTIME);
    writeln!(
        out,
        "      {:?}: $classOf,\n      {:?}: (a, b) => (a === b ? 1 : 0),\n      \
         {:?}: new WebAssembly.Suspending(async (promise, at) => (await promise)(at)),",
        ExternHostQuery::ClassOf.import_name(),
        ExternHostQuery::Same.import_name(),
        ExternHostQuery::PromiseWait.import_name()
    )
    .unwrap();

    let functions = module
        .resources
        .iter()
        .flat_map(|r| &r.methods)
        .chain(module.interfaces.iter().flat_map(|i| &i.functions));
    for function in functions {
        write_function(&mut out, function, &output.js[&function.cm_attr]);
    }
    out.push_str("    },\n  };\n}\n");
    out
}

/// Whether a list's elements may be of type `ty`: what [`load`] reads and
/// [`store`] writes at one address.
#[must_use]
pub fn list_carries(ty: &WadoType) -> bool {
    match Shape::of(ty) {
        Shape::Scalar(_)
        | Shape::Enum(_)
        | Shape::String
        | Shape::Record(_)
        | Shape::Variant(_) => true,
        Shape::List(_) | Shape::Option(_) | Shape::Callback(_) => false,
    }
}

/// Whether a shim can read `params`: as core values, or from memory where
/// none is a closure.
#[must_use]
pub fn reads_params(params: &[WadoParam]) -> bool {
    !cross_in_memory(params)
        || !params
            .iter()
            .any(|p| matches!(Shape::of(&p.ty), Shape::Callback(_)))
}

/// Whether `params` are more core values than a call takes, so that they
/// cross as the address of a record holding them.
fn cross_in_memory(params: &[WadoParam]) -> bool {
    let flat_params: usize = params.iter().map(|p| flat_count(&p.ty, Place::Whole)).sum();
    flat_params_in_buffer(flat_params, false)
}

fn write_function(out: &mut String, function: &WadoFunction, js: &JsFunction) {
    assert!(
        reads_params(&function.params),
        "`{}` passes a closure through memory, which no shim reads",
        function.cm_attr
    );
    let mut flat = FlatParams::default();
    let mut args: Vec<String> = if cross_in_memory(&function.params) {
        let record = flat.next();
        let (offsets, ..) = record_layout(function.params.iter().map(|p| &p.ty));
        function
            .params
            .iter()
            .zip(offsets)
            .map(|(p, offset)| load(&p.ty, &record, offset))
            .collect()
    } else {
        function
            .params
            .iter()
            .map(|p| lift(&p.ty, &mut flat, Place::Whole))
            .collect()
    };
    if js.variadic {
        let last = args.last_mut().expect("a variadic argument");
        *last = format!("...{last}");
    }
    let receiver = || args.first().expect("a member of the receiver");
    let call_args = |skip: usize| args[skip..].join(", ");
    let value = match &js.member {
        JsMember::Get(attribute) => format!("{}.{attribute}", receiver()),
        JsMember::Set(attribute) => format!("{}.{attribute} = {}", receiver(), args[1]),
        JsMember::Call(operation) => format!("{}.{operation}({})", receiver(), call_args(1)),
        JsMember::Static { interface, name } => {
            format!("globalThis.{interface}.{name}({})", call_args(0))
        }
        JsMember::New { interface } => format!("new globalThis.{interface}({})", call_args(0)),
        JsMember::Global => "globalThis".to_string(),
        JsMember::GlobalGet(attribute) => format!("globalThis.{attribute}"),
        JsMember::ReasonName => format!("String({}.name ?? \"Error\")", receiver()),
        JsMember::ReasonMessage => format!("String({0}.message ?? {0})", receiver()),
    };
    let body = match &function.return_type {
        Some(WadoType::Result { ok, .. }) if function.is_async => {
            // The reason's slot and the settled value share the payload.
            let (_, align) = ok.as_deref().map_or((0, 1), layout);
            let payload = align_to(1, align.max(Scalar::Handle.size()));
            let mut lines = vec![format!(
                "return $settle({value}, {payload}, ($ret, $value) => {{"
            )];
            if let Some(ok) = ok {
                let mut written = Vec::new();
                store(ok, "$ret", 0, "$value", &mut written);
                lines.extend(written.into_iter().map(|line| format!("  {line}")));
            }
            lines.push("});".to_string());
            lines
        }
        None => vec![format!("{value};")],
        Some(ty) if flat_count(ty, Place::Whole) == 1 => {
            vec![format!("return {};", lower(ty, &value, Place::Whole))]
        }
        // A result of more than one core value returns through memory, at the
        // address the last parameter carries.
        Some(ty) => {
            flat.names.push("$ret".to_string());
            let mut lines = vec![format!("const $value = {value};")];
            store(ty, "$ret", 0, "$value", &mut lines);
            lines
        }
    };
    writeln!(
        out,
        "      {:?}: ({}) => {{",
        cm_import_alias(&function.cm_attr),
        flat.names.join(", ")
    )
    .unwrap();
    for line in body {
        writeln!(out, "        {line}").unwrap();
    }
    out.push_str("      },\n");
}

/// The core parameters of the function being written, named in order.
#[derive(Default)]
struct FlatParams {
    names: Vec<String>,
}

/// The core values a value is lifted from, in order.
trait CoreValues {
    fn next(&mut self) -> String;
}

impl CoreValues for FlatParams {
    fn next(&mut self) -> String {
        let name = format!("p{}", self.names.len());
        self.names.push(name.clone());
        name
    }
}

/// The core values one case of a variant reads, each already of its own type.
struct CaseValues(std::vec::IntoIter<String>);

impl CoreValues for CaseValues {
    fn next(&mut self) -> String {
        self.0
            .next()
            .expect("a core value per value the case reads")
    }
}

/// The type of a core value.
#[derive(Clone, Copy, PartialEq)]
enum Core {
    I32,
    I64,
    F32,
    F64,
    Ref,
}

impl Core {
    /// The type holding either of two cases' values at one position, as the
    /// canonical ABI joins them.
    fn join(self, other: Self) -> Self {
        match (self, other) {
            _ if self == other => self,
            (Self::I32, Self::F32) | (Self::F32, Self::I32) => Self::I32,
            _ => Self::I64,
        }
    }

    /// `value`, held as a `joined`, read back as this type.
    fn read_from(self, joined: Self, value: &str) -> String {
        match (joined, self) {
            _ if joined == self => value.to_string(),
            (Self::I32, Self::F32) => format!("$f32({value})"),
            (Self::I64, Self::I32) => format!("Number(BigInt.asIntN(32, {value}))"),
            (Self::I64, Self::F32) => format!("$f32(Number(BigInt.asIntN(32, {value})))"),
            (Self::I64, Self::F64) => format!("$f64({value})"),
            _ => unreachable!("no join of core types holds one as another this way"),
        }
    }
}

/// A value that is one core value, and one slot in memory.
#[derive(Clone, Copy)]
enum Scalar {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// The object itself, as an `externref`.
    Handle,
}

/// How a type the `WebIDL` frontend lowers to crosses.
enum Shape<'a> {
    Scalar(Scalar),
    /// An enum of `$ENUMS`, by its name there: the index of its case.
    Enum(&'a str),
    /// A dictionary: its members one after another.
    Record(&'a [DictionaryField]),
    /// A union: the index of a case, then that case's value.
    Variant(&'a [UnionCase]),
    String,
    List(&'a WadoType),
    Option(&'a WadoType),
    Callback(&'a [WadoType]),
}

impl<'a> Shape<'a> {
    fn of(ty: &'a WadoType) -> Self {
        match ty {
            WadoType::Borrow(inner) => Self::of(inner),
            WadoType::Bool => Self::Scalar(Scalar::Bool),
            WadoType::I8 => Self::Scalar(Scalar::I8),
            WadoType::I16 => Self::Scalar(Scalar::I16),
            WadoType::I32 => Self::Scalar(Scalar::I32),
            WadoType::I64 => Self::Scalar(Scalar::I64),
            WadoType::U8 => Self::Scalar(Scalar::U8),
            WadoType::U16 => Self::Scalar(Scalar::U16),
            WadoType::U32 => Self::Scalar(Scalar::U32),
            WadoType::U64 => Self::Scalar(Scalar::U64),
            WadoType::F32 => Self::Scalar(Scalar::F32),
            WadoType::F64 => Self::Scalar(Scalar::F64),
            WadoType::Named(_) => Self::Scalar(Scalar::Handle),
            WadoType::StringEnum(name) => Self::Enum(name),
            WadoType::Dictionary(dictionary) => Self::Record(&dictionary.fields),
            WadoType::Union(union) => Self::Variant(&union.cases),
            WadoType::String => Self::String,
            WadoType::List(elem) => Self::List(elem),
            WadoType::Option(inner) => Self::Option(inner),
            WadoType::Callback { params, .. } => Self::Callback(params),
            WadoType::Char
            | WadoType::I128
            | WadoType::U128
            | WadoType::Result { .. }
            | WadoType::HashMap(..)
            | WadoType::Tuple(_)
            | WadoType::Stream(_)
            | WadoType::Future(_) => {
                unreachable!("the WebIDL frontend lowers to no {ty:?} here")
            }
        }
    }
}

fn is_handle(ty: &WadoType) -> bool {
    matches!(Shape::of(ty), Shape::Scalar(Scalar::Handle))
}

/// Where a value crossing as core values sits, which decides what a handle is.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    /// A parameter or a result itself: a handle is the object, and an `Option`
    /// of one the object or the null reference.
    Whole,
    /// Inside a record or an `Option`: a handle is the index of its slot, as
    /// it is in memory.
    Part,
}

/// How many core values `ty` crosses as, at `place`.
fn flat_count(ty: &WadoType, place: Place) -> usize {
    flat(ty, place).len()
}

/// The core values `ty` crosses as, at `place`.
fn flat(ty: &WadoType, place: Place) -> Vec<Core> {
    match Shape::of(ty) {
        Shape::Option(inner) if place == Place::Whole && is_handle(inner) => vec![Core::Ref],
        Shape::Option(inner) => [vec![Core::I32], flat(inner, Place::Part)].concat(),
        Shape::String | Shape::List(_) => vec![Core::I32; 2],
        Shape::Scalar(Scalar::Handle) if place == Place::Whole => vec![Core::Ref],
        Shape::Scalar(scalar) => vec![scalar.core()],
        Shape::Enum(_) => vec![Core::I32],
        Shape::Callback(_) => vec![Core::Ref],
        Shape::Record(fields) => fields
            .iter()
            .flat_map(|field| flat(&field.ty, Place::Part))
            .collect(),
        Shape::Variant(cases) => [vec![Core::I32], joined(cases)].concat(),
    }
}

/// The core values the payloads of `cases` share: as many as the longest
/// takes, each wide enough for every case's value there.
fn joined(cases: &[UnionCase]) -> Vec<Core> {
    let mut joined: Vec<Core> = Vec::new();
    for case in cases {
        for (at, core) in flat(&case.ty, Place::Part).into_iter().enumerate() {
            match joined.get_mut(at) {
                Some(held) => *held = held.join(core),
                None => joined.push(core),
            }
        }
    }
    joined
}

/// The host's value of whichever of `cases` has the index `index`, each read
/// by `read`.
fn one_of(cases: &[UnionCase], index: &str, mut read: impl FnMut(&UnionCase) -> String) -> String {
    let (last, rest) = cases.split_last().expect("a union has cases");
    let mut choice = String::from("(");
    for (at, case) in rest.iter().enumerate() {
        write!(choice, "{index} === {at} ? {} : ", read(case)).unwrap();
    }
    choice.push_str(&read(last));
    choice.push(')');
    choice
}

/// The offset of a variant's payload after its case's index, and the variant's
/// size and alignment.
fn variant_layout(cases: &[UnionCase]) -> (u32, u32, u32) {
    assert!(cases.len() <= 1 << 8, "a case's index is one byte");
    let (size, align) = cases
        .iter()
        .map(|case| layout(&case.ty))
        .fold((0, 1), |(size, align), (case_size, case_align)| {
            (size.max(case_size), align.max(case_align))
        });
    let payload = align_to(1, align);
    (payload, align_to(payload + size, align), align)
}

/// The host's object of `fields`, each member read by `read`.
fn object(fields: &[DictionaryField], mut read: impl FnMut(&WadoType) -> String) -> String {
    let members: Vec<String> = fields
        .iter()
        .map(|field| format!("{}: {}", field.host_name, read(&field.ty)))
        .collect();
    format!("{{ {} }}", members.join(", "))
}

/// The offset of each of `types` in a record of them, and the record's size
/// and alignment.
fn record_layout<'a>(types: impl Iterator<Item = &'a WadoType>) -> (Vec<u32>, u32, u32) {
    let mut end = 0;
    let mut record_align = 1;
    let offsets = types
        .map(|ty| {
            let (size, align) = layout(ty);
            let offset = align_to(end, align);
            end = offset + size;
            record_align = record_align.max(align);
            offset
        })
        .collect();
    (offsets, align_to(end, record_align), record_align)
}

fn field_types(fields: &[DictionaryField]) -> impl Iterator<Item = &WadoType> {
    fields.iter().map(|field| &field.ty)
}

/// The host's value of the enum `name` whose case has the index `index`.
fn enum_value(name: &str, index: &str) -> String {
    format!("$ENUMS.{name}[{index}]")
}

/// The index of the case the host's `value` of the enum `name` is.
fn enum_index(name: &str, value: &str) -> String {
    format!("$case($ENUMS.{name}, {value})")
}

/// The DOM value of a parameter of type `ty`, read from the core parameters it
/// takes from `flat`. A `None` is `undefined`, which the DOM reads as an
/// argument left out.
fn lift(ty: &WadoType, flat: &mut dyn CoreValues, place: Place) -> String {
    match Shape::of(ty) {
        Shape::Option(inner) if place == Place::Whole && is_handle(inner) => {
            format!("({} ?? undefined)", flat.next())
        }
        Shape::Option(inner) => {
            let case = flat.next();
            let some = lift(inner, flat, Place::Part);
            format!("({case} === 0 ? undefined : {some})")
        }
        Shape::String => {
            let (ptr, len) = (flat.next(), flat.next());
            format!("$string({ptr}, {len})")
        }
        Shape::List(elem) => {
            let (ptr, len) = (flat.next(), flat.next());
            let (size, _) = layout(elem);
            format!(
                "$list({ptr}, {len}, {size}, (at) => {})",
                load(elem, "at", 0)
            )
        }
        Shape::Callback(params) => callback(params, &flat.next()),
        Shape::Scalar(scalar) if place == Place::Part => scalar.read_part(&flat.next()),
        Shape::Scalar(scalar) => scalar.read_core(&flat.next()),
        Shape::Enum(name) => enum_value(name, &flat.next()),
        Shape::Record(fields) => object(fields, |ty| lift(ty, flat, Place::Part)),
        Shape::Variant(cases) => {
            let index = flat.next();
            let shared: Vec<(Core, String)> = joined(cases)
                .into_iter()
                .map(|core| (core, flat.next()))
                .collect();
            one_of(cases, &index, |case| {
                let own = self::flat(&case.ty, Place::Part);
                let values: Vec<String> = own
                    .into_iter()
                    .zip(&shared)
                    .map(|(core, (held, value))| core.read_from(*held, value))
                    .collect();
                lift(&case.ty, &mut CaseValues(values.into_iter()), Place::Part)
            })
        }
    }
}

/// The one core value the DOM's `value` of type `ty` returns as, at `place`.
fn lower(ty: &WadoType, value: &str, place: Place) -> String {
    match Shape::of(ty) {
        // An `undefined` would cross as a reference that is not null.
        Shape::Option(inner) if place == Place::Whole && is_handle(inner) => {
            format!("({value} ?? null)")
        }
        Shape::Scalar(scalar) if place == Place::Part => scalar.to_part(value),
        Shape::Scalar(scalar) => scalar.to_core(value),
        Shape::Enum(name) => enum_index(name, value),
        Shape::Record([only]) => lower(
            &only.ty,
            &format!("{value}.{}", only.host_name),
            Place::Part,
        ),
        Shape::Record(_)
        | Shape::Variant(_)
        | Shape::Option(_)
        | Shape::String
        | Shape::List(_)
        | Shape::Callback(_) => {
            unreachable!("{ty:?} is no single core value")
        }
    }
}

/// The function calling the closure `closure` back, which takes `params`.
fn callback(params: &[WadoType], closure: &str) -> String {
    let words = params.iter().map(|ty| {
        ty.callback_argument_word().unwrap_or_else(|| {
            unreachable!("the WebIDL frontend admits no {ty:?} callback argument")
        })
    });
    let export = cm_callback_func_name(&callback_export_name(words));
    let names: Vec<String> = (0..params.len()).map(|i| format!("a{i}")).collect();
    let lowered: Vec<String> = params
        .iter()
        .zip(&names)
        .map(|(ty, name)| format!(", {}", lower(ty, name, Place::Whole)))
        .collect();
    let taken: Vec<String> = names.iter().map(|name| format!(", {name}")).collect();
    format!(
        "$callback({closure}, (closure{}) => $enter({export:?})(closure{}))",
        taken.concat(),
        lowered.concat()
    )
}

/// The canonical ABI's size and alignment of `ty` in linear memory.
fn layout(ty: &WadoType) -> (u32, u32) {
    match Shape::of(ty) {
        Shape::Scalar(scalar) => {
            let size = scalar.size();
            (size, size)
        }
        Shape::Enum(_) => (1, 1),
        Shape::Record(fields) => {
            let (_, size, align) = record_layout(field_types(fields));
            (size, align)
        }
        Shape::Variant(cases) => {
            let (_, size, align) = variant_layout(cases);
            (size, align)
        }
        Shape::String | Shape::List(_) => (8, 4),
        Shape::Option(inner) => {
            let (size, align) = layout(inner);
            (align_to(align_to(1, align) + size, align), align)
        }
        Shape::Callback(_) => unreachable!("a closure crosses only as a whole parameter"),
    }
}

fn align_to(offset: u32, align: u32) -> u32 {
    offset.div_ceil(align) * align
}

/// The address `offset` past `base`, as the glue writes it.
fn place(base: &str, offset: u32) -> String {
    if offset == 0 {
        base.to_string()
    } else {
        format!("{base} + {offset}")
    }
}

/// The address and the length a string or a list holds at `at`.
fn span(at: &str) -> (String, String) {
    (
        format!("$view().getUint32({at}, true)"),
        format!("$view().getUint32({at} + 4, true)"),
    )
}

/// The DOM value of type `ty` read from linear memory at `base + offset`. A
/// `None` is `undefined`, as [`lift`] reads one.
fn load(ty: &WadoType, base: &str, offset: u32) -> String {
    let at = place(base, offset);
    match Shape::of(ty) {
        Shape::String => {
            let (ptr, len) = span(&at);
            format!("$string({ptr}, {len})")
        }
        Shape::Scalar(scalar) => scalar.read_memory(&at),
        Shape::Enum(name) => enum_value(name, &Scalar::U8.read_memory(&at)),
        Shape::Option(inner) => {
            let (_, align) = layout(inner);
            let some = load(inner, base, offset + align_to(1, align));
            format!(
                "({} === 0 ? undefined : {some})",
                Scalar::U8.read_memory(&at)
            )
        }
        Shape::List(elem) => {
            let (ptr, len) = span(&at);
            let (size, _) = layout(elem);
            format!(
                "$list({ptr}, {len}, {size}, (at) => {})",
                load(elem, "at", 0)
            )
        }
        Shape::Record(fields) => {
            let (offsets, ..) = record_layout(field_types(fields));
            let mut offsets = offsets.into_iter();
            let read = object(fields, |ty| {
                let field_offset = offsets.next().expect("an offset per field");
                load(ty, base, offset + field_offset)
            });
            format!("({read})")
        }
        Shape::Variant(cases) => {
            let (payload, ..) = variant_layout(cases);
            one_of(cases, &Scalar::U8.read_memory(&at), |case| {
                load(&case.ty, base, offset + payload)
            })
        }
        Shape::Callback(_) => unreachable!("a closure crosses only as a whole parameter"),
    }
}

/// The statements writing the DOM's `value` of type `ty` at `base + offset`.
fn store(ty: &WadoType, base: &str, offset: u32, value: &str, lines: &mut Vec<String>) {
    let at = place(base, offset);
    match Shape::of(ty) {
        Shape::String => lines.push(format!("$putString({at}, {value});")),
        Shape::Option(inner) => {
            let (_, align) = layout(inner);
            lines.push(format!("if ({value} === null || {value} === undefined) {{"));
            lines.push(format!("  $view().setUint8({at}, 0);"));
            lines.push("} else {".to_string());
            lines.push(format!("  $view().setUint8({at}, 1);"));
            let mut some = Vec::new();
            store(inner, base, offset + align_to(1, align), value, &mut some);
            lines.extend(some.into_iter().map(|line| format!("  {line}")));
            lines.push("}".to_string());
        }
        Shape::Scalar(scalar) => lines.push(scalar.to_memory(&at, value)),
        Shape::Enum(name) => lines.push(Scalar::U8.to_memory(&at, &enum_index(name, value))),
        Shape::Record(fields) => {
            let (offsets, ..) = record_layout(field_types(fields));
            for (field, field_offset) in fields.iter().zip(offsets) {
                let member = format!("{value}.{}", field.host_name);
                store(&field.ty, base, offset + field_offset, &member, lines);
            }
        }
        Shape::List(elem) => {
            let (size, align) = layout(elem);
            lines.push(format!(
                "$putList({at}, {value}, {size}, {align}, (at, item) => {{"
            ));
            let mut items = Vec::new();
            store(elem, "at", 0, "item", &mut items);
            lines.extend(items.into_iter().map(|line| format!("  {line}")));
            lines.push("});".to_string());
        }
        Shape::Variant(_) | Shape::Callback(_) => {
            unreachable!("the WebIDL frontend returns no {ty:?}")
        }
    }
}

impl Scalar {
    /// The DOM value that the core value `value` is.
    fn read_core(self, value: &str) -> String {
        match self {
            Self::Bool => format!("({value} !== 0)"),
            // A core `i32` reaches JavaScript signed.
            Self::U32 => format!("({value} >>> 0)"),
            Self::I64 => format!("Number({value})"),
            Self::U64 => format!("Number(BigInt.asUintN(64, {value}))"),
            Self::I8
            | Self::I16
            | Self::I32
            | Self::U8
            | Self::U16
            | Self::F32
            | Self::F64
            | Self::Handle => value.to_string(),
        }
    }

    /// The core value the DOM's `value` crosses as.
    fn to_core(self, value: &str) -> String {
        match self {
            Self::Bool => format!("({value} ? 1 : 0)"),
            Self::I64 | Self::U64 => format!("BigInt({value})"),
            Self::I8
            | Self::I16
            | Self::I32
            | Self::U8
            | Self::U16
            | Self::U32
            | Self::F32
            | Self::F64
            | Self::Handle => value.to_string(),
        }
    }

    /// The DOM value that `value`, a part of a larger one, is. A handle there
    /// is the index of its slot, which the guest frees.
    fn read_part(self, value: &str) -> String {
        if matches!(self, Self::Handle) {
            format!("$core.exports[$SLOT_GET]({value})")
        } else {
            self.read_core(value)
        }
    }

    /// What the DOM's `value` is as a part of a larger one. A handle goes into
    /// a slot, which the guest frees where it takes the handle out.
    fn to_part(self, value: &str) -> String {
        if matches!(self, Self::Handle) {
            format!("$core.exports[$SLOT_PUT]({value})")
        } else {
            self.to_core(value)
        }
    }

    /// The DOM value in linear memory at `at`.
    fn read_memory(self, at: &str) -> String {
        self.read_part(&format!("$view().{}", self.accessor("get", at, None)))
    }

    /// The statement writing the DOM's `value` at `at`.
    fn to_memory(self, at: &str, value: &str) -> String {
        let written = self.to_part(value);
        format!("$view().{};", self.accessor("set", at, Some(&written)))
    }

    /// The core value it is as a part of a larger value.
    fn core(self) -> Core {
        match self {
            Self::Bool
            | Self::I8
            | Self::I16
            | Self::I32
            | Self::U8
            | Self::U16
            | Self::U32
            | Self::Handle => Core::I32,
            Self::I64 | Self::U64 => Core::I64,
            Self::F32 => Core::F32,
            Self::F64 => Core::F64,
        }
    }

    /// Its size in linear memory, which is also its alignment.
    fn size(self) -> u32 {
        match self {
            Self::Bool | Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 | Self::Handle => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    /// The `DataView` call reading it at `at`, or writing `value` there.
    fn accessor(self, verb: &str, at: &str, value: Option<&str>) -> String {
        let method = match self {
            Self::Bool | Self::U8 => "Uint8",
            Self::I8 => "Int8",
            Self::I16 => "Int16",
            Self::U16 => "Uint16",
            Self::I32 => "Int32",
            Self::U32 | Self::Handle => "Uint32",
            Self::I64 => "BigInt64",
            Self::U64 => "BigUint64",
            Self::F32 => "Float32",
            Self::F64 => "Float64",
        };
        let mut args = vec![at.to_string()];
        args.extend(value.map(str::to_string));
        if self.size() > 1 {
            args.push("true".to_string());
        }
        format!("{verb}{method}({})", args.join(", "))
    }
}
