# Static Reflection

Reflection lets library code read a type's name and structure. The compiler
describes each type through a family of sealed traits, and a library derives a
trait from that description with an ordinary generic `impl`. Every answer is
resolved at compile time, and nothing is looked up at run time.

## The `Reflect` Family

The family has one root and one trait per kind of type. The root carries a
type's identity. Each kind trait carries the structure of that kind.

| Trait             | Implemented for                | Carries                                                                        |
| ----------------- | ------------------------------ | ------------------------------------------------------------------------------ |
| `Reflect`         | every type below               | `type_name()`, `wire_name_policy()`                                            |
| `ReflectStruct`   | every struct                   | `FieldTypes`, `FieldSlots`, `Members`, `members()`, `from_fields()`, the slots |
| `ReflectVariant`  | every variant                  | `CasePayloads`, `Members`, `members()`, `discriminant()`                       |
| `ReflectEnum`     | every enum                     | `Members`, `members()`, `discriminant()`, `from_discriminant()`                |
| `ReflectFlags`    | every flags type               | `Members`, `members()`, `bits()`, `from_bits()`                                |
| `ReflectNewtype`  | every newtype                  | `Base`                                                                         |
| `ReflectTemplate` | the shape of a tagged template | see [Tagged Template Literals](./spec-literals.md#tagged-template-literals)    |

The prelude declares all of them, so a bound names one with no import.

Each kind trait has `Reflect` as a
[supertrait](./spec-traits.md#supertraits). So a kind bound alone reaches the
root's methods:

<!-- {"fixture":"spec_reflection_family.wado"} -->

```wado
// `T: ReflectStruct` alone reaches `Reflect`'s methods
fn struct_name<T: ReflectStruct>() -> String {
    return Reflect::<T>::type_name();
}

test "a kind bound reaches the root" {
    assert struct_name::<Point>() == "Point";
}
```

### Trait-Qualified Calls

A reflection method is called through its trait, as
`Reflect::<T>::type_name()` or `ReflectStruct::<T>::members()`. The type's own
namespace stays its author's, so a type may declare a function of the same name:

<!-- {"fixture":"spec_reflection_family.wado"} -->

```wado
struct Tagged {
    tag: i32,
}

impl Tagged {
    fn type_name() -> String {    // the type's own function, not reflection's
        return "tagged";
    }
}

test "reflection leaves the type's namespace alone" {
    assert Tagged::type_name() == "tagged";
    assert Reflect::<Tagged>::type_name() == "Tagged";
}
```

A reflection trait takes exactly one type argument, the subject:

<!-- {"fixture":"spec_reflection_subject_arity_error.wado"} -->

```wado
let n = Reflect::<Point, i32>::type_name();  // ERROR: one subject, found 2
```

### Sealed Traits

Only the compiler implements a reflection trait. An `impl` of one is a compile
error:

<!-- {"fixture":"spec_reflection_sealed_error.wado"} -->

```wado
type Meters = f64;

impl ReflectNewtype for Meters {  // ERROR: cannot implement `ReflectNewtype`
    type Base = f64;
}
```

A member handle is sealed the same way. Its fields are private, so only
`members()` creates one:

<!-- {"fixture":"spec_reflection_forged_handle_error.wado"} -->

```wado
let f: EnumCase<Point> = EnumCase {  // ERROR: field `discriminant` of struct
    discriminant: 0,                 //        `EnumCase` is private
    value: Point { x: 0 },
    case_name: "x",
    wire_override: null,
};
```

## Identity

### Type Names

`Reflect::<T>::type_name()` returns the name of the declaration. A generic type
answers without its type arguments:

<!-- {"fixture":"spec_reflection_type_name.wado"} -->

```wado
test "a generic type is named without its arguments" {
    assert Reflect::<Pair<i32>>::type_name() == "Pair";
    assert Reflect::<Pair<String>>::type_name() == "Pair";
    assert Reflect::<Option<i32>>::type_name() == "Option";
}
```

A newtype answers with its own name, not its base's. In a chain, each link
answers for itself:

<!-- {"fixture":"spec_reflection_type_name.wado"} -->

```wado
type Meters = f64;
type Distance = Meters;

test "a newtype names its own link of the chain" {
    assert Reflect::<Meters>::type_name() == "Meters";
    assert Reflect::<Distance>::type_name() == "Distance";
}
```

### Wire Name Policy

`Reflect::<T>::wire_name_policy()` returns the type's
[`#[wire(name_policy)]`](./spec-attributes.md#wire) as a `CaseStyle`, and
`CaseStyle::Identity` when the type declares none. A member's
`wire_name_override()` returns its `#[wire(name)]` as written, or `None`.
Reflection reports these facts and applies neither:

<!-- {"fixture":"spec_reflection_wire.wado"} -->

```wado
#[wire(name_policy = "camelCase")]
struct Account {
    user_id: i32,
    #[wire(name = "mail")]
    email_address: String,
}

struct Plain {
    user_id: i32,
}

test "the policy is reported, not applied" {
    assert Reflect::<Account>::wire_name_policy() == CaseStyle::Camel;
    assert Reflect::<Plain>::wire_name_policy() == CaseStyle::Identity;
    let fs = ReflectStruct::<Account>::members();
    assert fs.0.name() == "user_id";
    assert fs.0.wire_name_override() matches { None };
    assert fs.1.wire_name_override() matches { Some(n) && n == "mail" };
}
```

`core:serde`'s `wire_name(member, policy)` resolves a wire name from the two
facts. The override wins, and otherwise the policy's casing applies to the
source name:

<!-- {"fixture":"spec_reflection_wire.wado"} -->

```wado
test "core:serde resolves the wire name from both facts" {
    let fs = ReflectStruct::<Account>::members();
    let policy = Reflect::<Account>::wire_name_policy();
    assert wire_name(&fs.0, policy) == "userId";
    assert wire_name(&fs.1, policy) == "mail";
}
```

## Members

A kind with members declares `type Members` and `fn members()`. `members()`
returns a tuple with one handle per member, in declaration order. The handle
types are the prelude structs `StructField<T, F>`, `VariantCase<T, P>`,
`EnumCase<T>`, `FlagsBit<T>` and `TemplateHole<T, V>`. `T` is the subject.
`F`, `P` and `V` are the member's own type, where the member has one.

`members()` on a concrete subject is an ordinary tuple, so its elements are
reached as `.0`, `.1`, and so on. A walk over it is a
[tuple `for-of`](./spec-control-flow.md#tuple-for-of-compile-time-expansion), so
the body runs once per member and sees each handle at that member's own type.

### The `Member` Trait

Every handle implements the sealed `Member` trait. `name()` returns the source
name, and `wire_name_override()` the `#[wire(name)]` override. A helper written
over `Member` reads a member of any kind:

<!-- {"fixture":"spec_reflection_member.wado"} -->

```wado
fn label<M: Member>(m: &M) -> String {
    return `<${m.name()}>`;
}

test "one helper reads a member of any kind" {
    assert label(&ReflectStruct::<Point>::members().1) == "<y>";
    assert label(&ReflectEnum::<Color>::members().0) == "<Red>";
    assert label(&ReflectVariant::<Shape>::members().1) == "<Empty>";
    assert label(&ReflectFlags::<Perm>::members().1) == "<Write>";
}
```

A flags member cannot carry `#[wire]`, so its override is always `None`:

<!-- {"fixture":"spec_reflection_member.wado"} -->

```wado
test "a flags member has no wire override" {
    assert ReflectFlags::<Perm>::members().0.wire_name_override() matches { None };
}
```

### Structs

`ReflectStruct` describes a struct. `FieldTypes` is the tuple of the field
types, in declaration order. `members()` returns one `StructField<T, F>` per
field. A field handle answers `index()` (its position, from 0), `name()`,
`wire_name_override()`, `has_default()`, `is_secret()`, and `get(&v)`, which
returns the field's value in `v`:

<!-- {"fixture":"spec_reflection_struct.wado"} -->

```wado
struct Config {
    host: String,
    port: i32 = 8080,
    #[wire(name = "v")]
    verbose: bool,
}

test "members() walks the fields in declaration order" {
    let c = Config { host: "example.org", port: 80, verbose: true };
    let mut out = "";
    for let f of ReflectStruct::<Config>::members() {
        out.push_str(`${f.index()}:${f.name()}=${f.get(&c):?} `);
    }
    assert out == "0:host=\"example.org\" 1:port=80 2:verbose=true ";
}

test "a handle reports what the field declares" {
    let fs = ReflectStruct::<Config>::members();
    assert !fs.0.has_default() && fs.1.has_default();
    assert fs.2.wire_name_override() matches { Some(n) && n == "v" };
}
```

`from_fields(fields)` builds the struct from a tuple of its field values, in
declaration order. It always succeeds, since the tuple is already typed:

<!-- {"fixture":"spec_reflection_struct.wado"} -->

```wado
test "from_fields builds the struct from its field values" {
    let c = ReflectStruct::<Config>::from_fields(["localhost", 3000, false]);
    assert c.host == "localhost" && c.port == 3000 && !c.verbose;
}
```

`FieldSlots` is `FieldTypes` with each element under `Option`. `empty_slots()`
returns every slot empty. `default_slot(i)` returns every slot empty except slot
`i`, which holds field `i`'s declared default, if the field has one:

<!-- {"fixture":"spec_reflection_struct.wado"} -->

```wado
test "the slots start empty and default_slot fills one default" {
    let empty = ReflectStruct::<Config>::empty_slots();
    assert empty.0 matches { None } && empty.1 matches { None };
    let slots = ReflectStruct::<Config>::default_slot(1);
    assert slots.1 matches { Some(8080) };
    assert slots.0 matches { None } && slots.2 matches { None };
}
```

`default_slot(i)` evaluates field `i`'s default and no other:

<!-- {"fixture":"spec_reflection_struct.wado"} -->

```wado
fn boom() -> i32 {
    panic("evaluated");
}

struct Lazy {
    first: i32 = boom(),
    second: i32 = 7,
}

test "default_slot evaluates only the default it fills" {
    assert ReflectStruct::<Lazy>::default_slot(1).1 matches { Some(7) };
}
```

A generic struct's `FieldTypes` is written in its own type parameters, and each
instantiation substitutes its type arguments:

<!-- {"fixture":"spec_reflection_struct.wado"} -->

```wado
struct Pair<T> {
    left: T,
    right: i32,
}

fn values<T: ReflectStruct<FieldTypes = [..F]>, ..F: Inspect>(v: &T) -> String {
    let mut out = "";
    for let f of ReflectStruct::<T>::members() {
        out.push_str(`${f.get(v):?};`);
    }
    return out;
}

test "a generic struct substitutes its arguments into FieldTypes" {
    assert values(&Pair { left: "ada", right: 7 }) == "\"ada\";7;";
    assert values(&Pair { left: 2.5, right: 7 }) == "2.5;7;";
}
```

### Variants

`ReflectVariant` describes a variant. `CasePayloads` is the tuple of the case
payload types, and a case without a payload contributes `()`.
`discriminant(&v)` returns the tag of `v`'s live case. `members()` returns one
`VariantCase<T, P>` per case.

A case handle answers `name()`, `wire_name_override()`, `discriminant()`,
`is_unit()`, and four value bridges:

- `holds(&v)` tells whether `v`'s live case is this case.
- `extract(&v)` returns the payload. It traps unless `holds(&v)`.
- `construct(payload)` builds this case around a payload.
- `make()` builds a case without a payload. It traps unless `is_unit()`.

<!-- {"fixture":"spec_reflection_variant.wado"} -->

```wado
variant Shape {
    Circle(f64),
    Rect([f64, f64]),
    Empty,
}

test "a case handle finds, reads and builds its case" {
    let s = Shape::Rect([3.0, 4.0]);
    assert ReflectVariant::<Shape>::discriminant(&s) == 1;
    let mut out = "";
    for let c of ReflectVariant::<Shape>::members() {
        if c.holds(&s) {
            out = `${c.name()} ${c.discriminant()} ${c.extract(&s):?}`;
        }
    }
    assert out == "Rect 1 [3.0, 4.0]";
    let cs = ReflectVariant::<Shape>::members();
    assert cs.0.construct(2.5) == Shape::Circle(2.5);
}
```

`make()` exists because a walk writes one body for every case, and
`construct(())` type-checks only where the payload is `()`:

<!-- {"fixture":"spec_reflection_variant.wado"} -->

```wado
test "a unit case has the payload () and is built by make()" {
    let cs = ReflectVariant::<Shape>::members();
    assert cs.2.is_unit() && !cs.0.is_unit();
    assert cs.2.make() == Shape::Empty;
    assert cs.2.construct(()) == Shape::Empty;
}
```

A bridge used on the wrong case traps:

<!-- {"fixture":"spec_reflection_variant.wado"} -->

```wado
#[expect_trap]
test "extract traps on a case the value does not hold" {
    let cs = ReflectVariant::<Shape>::members();
    let _ = cs.0.extract(&Shape::Empty);
}

#[expect_trap]
test "make traps on a case that carries a payload" {
    let cs = ReflectVariant::<Shape>::members();
    let _ = cs.0.make();
}
```

### Enums

`ReflectEnum` describes an enum. `discriminant(&v)` returns `v`'s tag.
`from_discriminant(tag)` returns the case with that tag, or `None` for a tag no
case has:

<!-- {"fixture":"spec_reflection_enum_flags.wado"} -->

```wado
test "an enum reads and builds a value by its discriminant" {
    assert ReflectEnum::<Color>::discriminant(&Color::Blue) == 2;
    assert ReflectEnum::<Color>::from_discriminant(1) == Option::Some(Color::Green);
    assert ReflectEnum::<Color>::from_discriminant(7) matches { None };
}
```

`members()` returns one `EnumCase<T>` per case. A case handle answers `name()`,
`wire_name_override()`, `discriminant()`, `holds(&v)`, and `make()`, which
returns the case's value:

<!-- {"fixture":"spec_reflection_enum_flags.wado"} -->

```wado
test "an enum case handle holds its value" {
    let cs = ReflectEnum::<Color>::members();
    assert cs.1.name() == "Green" && cs.1.discriminant() == 1;
    assert cs.1.holds(&Color::Green) && !cs.1.holds(&Color::Red);
    assert cs.1.make() == Color::Green;
}
```

### Flags

`ReflectFlags` describes a flags type. `bits(&v)` returns `v`'s bits
as a `u64`, whatever the type's width. `from_bits(raw)` returns the value with
those bits, or `None` when a bit no member owns is set:

<!-- {"fixture":"spec_reflection_enum_flags.wado"} -->

```wado
test "a flags value reads and builds its bits as a u64" {
    let p = Perm::Read | Perm::Exec;
    assert ReflectFlags::<Perm>::bits(&p) == 5;
    assert ReflectFlags::<Perm>::from_bits(3) == Option::Some(Perm::Read | Perm::Write);
    assert ReflectFlags::<Perm>::from_bits(8) matches { None };
}
```

`members()` returns one `FlagsBit<T>` per member. A bit handle answers
`name()`, `bit()` (its mask as a `u64`), `is_set(&v)`, and `set()`, which
returns the value with only this bit set:

<!-- {"fixture":"spec_reflection_enum_flags.wado"} -->

```wado
test "a flags bit handle holds its single-bit value" {
    let p = Perm::Read | Perm::Exec;
    let mut set = "";
    for let b of ReflectFlags::<Perm>::members() {
        if b.is_set(&p) {
            set.push_str(`${b.name()}=${b.bit()} `);
        }
    }
    assert set == "Read=1 Exec=4 ";
    assert ReflectFlags::<Perm>::members().1.set() == Perm::Write;
}
```

### Newtypes

`ReflectNewtype` describes a newtype. It has no members. `Base` names the
type it wraps, and a derivation crosses between the two with `as`:

<!-- {"fixture":"spec_reflection_newtype.wado"} -->

```wado
trait Doubled with () {
    fn doubled(&self) -> Self;
}

impl<N: ReflectNewtype<Base = B>, B: Add<Output = B>> Doubled for N {
    fn doubled(&self) -> N {
        let base = *self as B;
        return (base + base) as N;
    }
}

type Meters = f64;
type Name = String;

test "a derivation reads the base and casts across it" {
    assert (2.5 as Meters).doubled() == 5.0 as Meters;
    assert ("ab" as Name).doubled() == "abab" as Name;
}
```

A newtype [inherits its base's impls](./spec-types.md#inherited-associated-functions-and-traits),
and its base's kind trait is one of them. So a newtype over a struct walks the
struct's fields, while its name stays its own:

<!-- {"fixture":"spec_reflection_newtype.wado"} -->

```wado
struct Point {
    x: i32,
    y: i32,
}

type Spot = Point;

test "a newtype names itself and walks its base's members" {
    assert Reflect::<Spot>::type_name() == "Spot";
    let mut names = "";
    for let f of ReflectStruct::<Spot>::members() {
        names.push_str(f.name());
    }
    assert names == "xy";
}
```

### Templates

A tagged template's shape is its own kind, described by `ReflectTemplate`.
[Tagged Template Literals](./spec-literals.md#tagged-template-literals) states
its rules and its hole handles.

## Deriving a Trait

A library derives a trait for every type of one kind with a blanket `impl`
bounded by that kind's trait. The bound binds the payload types as a
[type pack](./spec-functions.md#variadic-type-packs) (`FieldTypes = [..F]`), and
a bound on the pack (`..F: Show`) lets the body call the trait on each value.
A field whose type is itself a struct takes the same blanket, so the derivation
recurses. The leaves, here `i32` and `String`, take impls written for them:

<!-- {"fixture":"spec_reflection_derive.wado"} -->

```wado
trait Show with () {
    fn show(&self) -> String;
}

impl Show for i32 {
    fn show(&self) -> String { return `${*self}`; }
}

impl Show for String {
    fn show(&self) -> String { return *self; }
}

impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: Show> Show for T {
    fn show(&self) -> String {
        let mut out = `${Reflect::<T>::type_name()}(`;
        for let f of ReflectStruct::<T>::members() {
            out.push_str(`${f.name()}=${f.get(self).show()};`);
        }
        return `${out})`;
    }
}

struct Point {
    x: i32,
    y: i32,
}

struct Label {
    text: String,
    at: Point,
}

test "a field that is a struct takes the same blanket" {
    let l = Label { text: "origin", at: Point { x: 0, y: 0 } };
    assert l.show() == "Label(text=origin;at=Point(x=0;y=0;);)";
}
```

A struct binds `FieldTypes` and a variant binds `CasePayloads`. An enum case
and a flags bit carry no payload, so their derivation binds `Members` instead:

<!-- {"fixture":"spec_reflection_derive.wado"} -->

```wado
impl<T: ReflectEnum<Members = [..M]>, ..M> CaseName for T {
    fn case_name(&self) -> String {
        for let c of ReflectEnum::<T>::members() {
            if c.holds(self) {
                return c.name();
            }
        }
        return "";
    }
}

enum Color {
    Red,
    Green,
}

test "an enum derivation binds its members" {
    assert Color::Green.case_name() == "Green";
}
```

A walk in a generic body needs this pack. A bound that fixes the payload types
to a concrete tuple does not bind one, and the walk is an error:

<!-- {"fixture":"spec_reflection_concrete_pack_error.wado"} -->

```wado
fn count<T: ReflectStruct<FieldTypes = [i32]>>(v: &T) -> i32 {
    let mut n = 0;
    for let f of ReflectStruct::<T>::members() {  // ERROR: `T` needs `FieldTypes = [..F]`
        n += 1;
    }
    return n;
}
```

The pack's bound holds only if every payload type meets it. It is checked where
the subject becomes known, and the error names the type that fails:

<!-- {"fixture":"spec_reflection_field_bound_error.wado"} -->

```wado
fn total<T: ReflectStruct<FieldTypes = [..F]>, ..F: Weight>(v: &T) -> i32 {
    let mut n = 0;
    for let f of ReflectStruct::<T>::members() {
        n += f.get(v).weight();
    }
    return n;
}

struct Item {
    count: i32,
    name: String,
}

test {
    let n = total(&Item { count: 2, name: "pen" });  // ERROR: `String` is not `Weight`
}
```

A derivation that needs no value maps over the pack with `[..F::method()]`:

<!-- {"fixture":"spec_reflection_derive.wado"} -->

```wado
impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: Tag> Schema for T {
    fn schema() -> String {
        let mut out = "";
        for let t of [..F::tag()] {
            out.push_str(`${t} `);
        }
        return out;
    }
}

struct Row {
    id: i32,
    live: bool,
}

test "a static derivation maps over the field types without a value" {
    assert Row::schema() == "int bool ";
}
```

A derived static method is called on the type. On a variant, a name that is not
one of its cases reaches the trait:

<!-- {"fixture":"spec_reflection_derive.wado"} -->

```wado
impl<V: ReflectVariant<CasePayloads = [..P]>, ..P: Zero> Parse for V {
    fn parse(case_name: String) -> Option<V> {
        for let c of ReflectVariant::<V>::members() {
            if c.name() == case_name {
                return Option::Some(c.construct(P::zero()));
            }
        }
        return Option::None;
    }
}

variant Shape {
    Circle(i32),
    Empty,
}

test "a static method that is not a case name reaches the blanket" {
    assert Shape::parse("Circle") == Option::Some(Shape::Circle(0));
    assert Shape::parse("Empty") == Option::Some(Shape::Empty);
    assert Shape::parse("Square") matches { None };
}
```

A free function may carry the same bound. The caller names neither the subject
nor the pack, since both follow from the argument:

<!-- {"fixture":"spec_reflection_derive.wado"} -->

```wado
fn set_members<L: ReflectFlags<Members = [..M]>, ..M>(v: &L) -> String {
    let mut out = "";
    for let b of ReflectFlags::<L>::members() {
        if b.is_set(v) {
            out.push_str(b.name());
        }
    }
    return out;
}

flags Perm {
    Read,
    Write,
}

test "a free function takes the bound and its caller names neither parameter" {
    assert set_members(&(Perm::Read | Perm::Write)) == "ReadWrite";
}
```

## Visibility

A kind trait (`ReflectStruct` and its siblings) holds only where every member
of the subject is visible at the use site. The root `Reflect` names the type and
no member, so it holds anywhere, a type whose fields are private included. The
impls derived where the type is declared still see every field:

<!-- {"fixture":"spec_reflection_visibility.wado"} -->

```wado
test "a type with private fields still names itself" {
    let o = make_opaque();              // `Opaque { root: i32, size: i32 }`, both private
    assert name_of(&o) == "Opaque";
    assert Reflect::<Opaque>::type_name() == "Opaque";
    assert `${o:?}` == "Opaque { root: 0, size: 0 }";
}
```

## Secret Fields

A [`#[secret]`](./spec-attributes.md#secret) field's handle answers
`is_secret()` with `true`. A derivation that must not reveal the value tests it,
as the derived `Inspect` does:

<!-- {"fixture":"spec_reflection_secret.wado"} -->

```wado
impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: Inspect> Dump for T {
    fn dump(&self) -> String {
        let mut out = "";
        for let f of ReflectStruct::<T>::members() {
            if f.is_secret() {
                out.push_str(`${f.name()}=*** `);
            } else {
                out.push_str(`${f.name()}=${f.get(self):?} `);
            }
        }
        return out;
    }
}

struct Login {
    user: String,
    #[secret]
    password: String,
}

test "a derivation reads is_secret() and hides the value" {
    let l = Login { user: "ann", password: "hunter2" };
    assert l.dump() == "user=\"ann\" password=*** ";
}
```

Rationale: [WEP: Library-Defined Derivation over `Reflect*`](./wep-2026-06-13-reflect-derivation.md),
[WEP: Struct Walkability](./wep-2026-07-10-struct-walkability.md).
