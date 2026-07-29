# WEP 2026-07-29: Name namespaces as types

## Context

Every naming defect found while migrating to structured fq names was the same
mistake: a name from one namespace handed to a consumer that keys on another.
The compiler answers at least five distinct questions with a `String`:

| namespace        | example                            | who keys on it                                                        |
| ---------------- | ---------------------------------- | --------------------------------------------------------------------- |
| mangled identity | `core:prelude/list.wado/List<i32>` | `func_map`, emitted function names                                    |
| declaration name | `List`, `MyArray`, `Wrapper<i32>`  | `impl` headers, module scope, CM interface registry, go-to-definition |
| struct-list key  | bare head + qualified args         | the package's struct list                                             |
| template key     | `List`                             | monomorphize template registration                                    |
| instance key     | `List<i32>`                        | per-instantiation identity                                            |

Observed failures, all of this shape:

- A resource method looked up as `{head_key}::{method}` against a registry keyed
  by the declared `Resource::method` — no import adapter synthesized, WIR build
  hit an unresolved `Descriptor::open_at`.
- A reflect bound checked with a mangled head against module-scope lookups — every
  `ReflectEnum` / `ReflectFlags` / `Deserialize` blanket silently stopped holding.
- `T::method()` rewritten to `{mangle_type_name(T)}::{method}` and fed to
  `locate_static_method_impl`, which compares against what a header writes.
- `with_substituted_struct_name` given one name to answer both "what is the base?"
  and "what is the instance?".
- `function_id_for` putting the receiver's type arguments in the id and dropping
  the method's, collapsing `field<T>` onto `field<i32>`.
- `FqTypeName` rendering a tuple as `[]<i32,i32>` where every other mangler
  spells `[i32,i32]`.

Structuring `FqTypeName` fixed the _representation_ but not the failure mode,
because every accessor still hands back a `String` and `Receiver::head_key` /
`decl_key` are interchangeable to the type checker. Splitting them made the
distinction expressible; nothing makes it enforced. Each of the bugs above was
caught by a test, never by the compiler.

## Decision

Make the namespace part of the type, so a name from one cannot reach a consumer
that keys on another. Four changes, ordered so each is independently valuable
and the cheapest one catches the most.

### 1. Namespaced name types

```rust
pub struct MangledName(String);
pub struct DeclName(String);
pub struct StructListKey(String);
```

No `Deref<Target = str>`, no `AsRef<str>`, no `From<String>`. They are minted
only by the authority that knows the namespace — `FqTypeName` and `TypeTable` —
and converted only through named methods that state why the change is sound.
`Display` goes on `DeclName` alone, the one form meant for humans.

Registries then demand their own key type:

```rust
impl CmInterfaceRegistry {
    fn get_function(&self, key: &DeclPath) -> Option<&FunctionInfo>;
}
impl TraitEnv {
    fn impl_headers(&self, target: &DeclName) -> ...;
}
```

`DeclPath` is built as receiver + method, never with `format!("{head}::{method}")`,
so the assembly sites disappear along with the chance to assemble from the wrong
half.

This step alone makes five of the six failures above fail to compile.

### 2. `LocalMethodName` derives its rendered name

It stores `struct_name: String` alongside `receiver` and `struct_type_args`, with
an unenforced invariant `struct_name == receiver.mangle(struct_type_args)`.
`struct_name` becomes a method over the two structural fields, and the illegal
state is gone.

Measured before attempting it: with the divergence reported from
`to_mangled_name` on every name the compiler emits, the fixtures still open
produce **no** divergence — the invariant holds today. The monomorphized-struct
defect was `fq_type_name` not knowing the arguments, which recording them at the
instantiation site already fixed. So this step is mechanical and
behaviour-preserving, not the risky one it looked like: ~108 `.struct_name`
field reads become calls, and the field and its hand-written initialisers go.

### 3. One encoding of "which type owns this method"

`inherited_from_base: Option<TypeId>`, `struct_name` and `receiver` are three
encodings of the same fact, and the generic-newtype defect is them disagreeing.
Replace with one:

```rust
enum MethodOwner {
    Own(TypeIdentity),
    Inherited { via: TypeIdentity, owner: TypeIdentity },
}
```

Naming reads `owner`. The newtype-override question reads the discriminant,
rather than comparing a declaration name against an instantiated one — which is
what made that guard never fire.

### 4. Declaration and instantiation separated in the type table

```rust
enum TypeIdentity {
    Declaration(DeclId),
    Instance { decl: DeclId, args: Vec<TypeId> },
}
```

`ResolvedType::Struct` carries a `TypeIdentity`, never a pre-rendered name.
Today it carries `name: String` with the arguments spelled into it plus a
separate `base_name`, so "what is the base?" and "what are the args?" are not
both answerable — the defect that cost the most this session. `is_monomorphized`
and `base_name` are then redundant and go away.

`TypeHead::Builtin(String)` becomes an enum of the shapes that actually exist, so
`FqTypeName::builtin("List")` — a declaration passed off as a builtin, which the
current API accepts and which appears in a test today — stops compiling.

Re-scoped after steps 1–3 landed. The correctness this step was to buy is
already bought: `TypeTable::monomorphized_struct_args` records the arguments
where the instantiation happens, so `fq_type_name` answers both "what is the
base?" and "what are the args?" for these types today. What remains is making
the fused state _unrepresentable_ rather than merely corrected — worth doing,
but hardening rather than a fix, and the most expensive step by a wide margin
(~160 match sites and a change to interning keys).

It should therefore run on a green branch, not on top of open regressions: it is
the one step that changes behaviour, and diagnosing a regression is much harder
once every `ResolvedType::Struct` site has moved.

Attempted, then backed out: renaming the fields to force every read site to be
revisited produced 143 compile errors across 20 files, which is the mechanical
part and is fine. What stops it is not mechanical. `ResolvedType` is interned by
structural equality, and today a monomorphized struct's identity is the
_rendered_ string `TreeMap<String,i32>` — so two distinct-but-equivalent
argument `TypeId`s collapse onto one interned type. Holding `type_args:
Vec<TypeId>` in the variant keys identity on those ids instead, and the same two
arguments would mint two types where there was one. Such ids demonstrably exist:
`Monomorphizer::try_queue_function` exists to dedupe a blanket instance reached
from two dispatch sites whose derived args are "distinct-but-equivalent
`TypeId`s".

So 4b needs a decision first, not a rename:

- canonicalize each argument through the table before interning, making
  equivalent ids collapse by construction; or
- keep the rendered spelling as the interning identity and store the arguments
  beside it, which leaves the structure available and identity untouched — a
  much smaller behaviour delta, and possibly enough for what 4b is for.

The second is right, since what 4b buys is that the head and arguments are
separately readable, not that identity changes — and the arguments already are,
via `monomorphized_struct_args`. So under it, what is left of 4b is one thing:
`ResolvedType::Struct::name` holds a _rendered_ spelling for an instantiation
and a _declaration_ spelling for a declaration, and nothing stops a caller
reading the first as the second. That is the same hazard steps 1a and 1b closed
elsewhere, and it closes the same way — type the field `MangledName`, leaving
interning untouched because the newtype hashes and compares as its `String`.

Measured, with the `PartialEq<str>` / `Display` conveniences already in place:
179 sites need to say which spelling they are taking. That is mechanical and
carries no behaviour change, but it is a single uninterruptible edit — the crate
does not compile between the first site and the last.

In progress on this branch — the crate does not compile, by agreement, so the
work can continue from a real mid-point rather than a description of one.

The structural finding, which the earlier estimates missed: `ResolvedType`'s
nominal variants are matched together in or-patterns that bind one `name`
across `Struct | Enum | Variant | Newtype | Flags | Resource | GenericInstance`.
Typing only `Struct::name` splits those bindings, so each such group needs
`Struct` lifted into its own arm. That is not churn — it is the point. A
monomorphized struct is the one nominal type whose `name` is not a declaration
spelling, and the or-patterns were what let it be read as though it were.

Four such groups in `tir.rs` are split. Automated conversion — suffix accessors
and `MangledName::new` mints, both filtered to skip `{ name, .. }` shorthand so
patterns are never touched — took 179 errors to 87 and then converged. What
remains needs decisions, not conversions:

- 34 are the same or-pattern split, spread over `elaborator.rs` (7),
  `synthesis/traits.rs` (6), `wit_emit.rs` (4), `serde_synth.rs` (3),
  `monomorphize/state.rs` (3) and others. Each wants `Struct` lifted into its own
  arm whose body renders the name, exactly as the four in `tir.rs` now do.
- 11 map keys typed `(String, ModuleSource)` (or the reverse order) that have to
  become `(MangledName, ModuleSource)` — the point being that these indices are
  keyed on mangled spellings and say so.
- ~40 in a tail of individual mismatches.

Driving it from `rustc`'s own diagnostics was tried and does not work
unsupervised. A fixer that appends the right accessor at each primary span
cleared 70 of the 179, then stalled: the remaining classes need the expression
_wrapped_ (`&String` → `&MangledName`), not suffixed, and — the reason to stop —
spans that name a _pattern_ binding get the accessor spliced into the pattern,
which is not valid Rust. `ResolvedType::Struct { name.as_mangled_str(), .. }` is
what that produces. So the edit needs a hand on each site, or a rewriter that
understands pattern vs expression position; `rustc`'s machine-applicable
suggestions cover only 49 of them and some of those wrap in the newtype's
private tuple field.

### 4b, second attempt: remove the fusion instead of annotating it

The first attempt typed `Struct::name` as `MangledName`. It compiled and then
regressed — an O0 run hung and five fixtures started failing, typically "type
does not implement Eq". Clearing 179 type errors means inserting
`as_mangled_str()` widely, and every insertion hands a declaration-keyed
consumer a `&str` it accepts silently. The barrier was absent at exactly the
sites it existed for. Backed out.

The field asymmetry is the defect. `Struct` alone stored a rendered spelling
where every other nominal variant stores a declaration name, and
`GenericInstance` already models declaration-plus-arguments properly. So
`Struct` now carries `decl_name` + `type_args`, and the rendered spelling is
derived by `TypeTable::struct_rendered_name`. No fused name, so nothing to
mistake; no annotation, so no accessor that strips it.

The hazard in finishing this, and the reason it cannot be a bulk rename:
renaming the pattern binding `name` → `decl_name` **compiles everywhere and is
silently wrong** for a monomorphized struct, because the old `name` was the
rendered spelling there. Each of the ~108 sites has to say which it wants, and
the ones wanting the rendered form need the type table in scope to derive it —
rendering is no longer free. A site that binds `name` today and uses it for
identity or mangling wants `struct_rendered_name(decl_name, type_args)`; one
that uses it for a declaration lookup wants `decl_name`.

`make_monomorphized_struct` carries a `debug_assert_eq!` that the caller's
rendering matches what `struct_rendered_name` derives, so a divergence between
the two shows up in tests rather than as a wrong mangled name.

The rule for converting the remaining sites, derived while doing the core
accessors:

- `struct_rendered_name(decl_name, type_args)` is the **behaviour-preserving**
  answer. The old `name` was the rendered spelling, so this reproduces today's
  behaviour exactly. Default to it.
- `decl_name` is a **behaviour change**, and where it is right the old code was
  wrong — those are the WEP 2026-07-28 defects, now visible one at a time.

Many sites turn out not to need either. A recurring shape is
`FqTypeName::declared(module_source, name)` built from the _rendered_ name —
which is the fusion bug written out longhand. Those collapse to
`fq_type_name(id)`, which is structurally correct now that head and arguments
come off the type. Prefer that over threading `decl_name` and `type_args`
through by hand.

### Corrected: the real O0 number is 73, not 2

Progress was being reported from `reflect_`/`serde_`-filtered runs, which reached
2. A full O0 run says **1880 passed, 73 failed**: `http_*`, `httpbin_*`,
`generic_struct_1`, `effect_handler_*`, `field_forward_*` — areas the filtered
runs never touched, and which the central renderers this work changed
(`get_type_name_info`, `mangle_type_arg_for_generic`, the struct name index, WIR
type resolution) all reach.

Many of the 73 share one cause: `type \`TreeMap\` does not implement trait
\`IndexAssign<K>\``. Traced as far as both sides of that lookup, and they appear
to agree — the registration key is built from`get_type_name_static`, which
answers`TreeMap`for the header`impl<K, V> IndexAssign<K> for TreeMap<K, V>`,
and the query builds`Decl((core:collections, "TreeMap"))`(both measured). So
the mismatch is not the target key, and the next probe belongs on the trait side
of the pair —`IndexAssign`against`IndexAssign<K>` — rather than the type side.

### Where the second attempt stands

`ResolvedType::Struct` carries `decl_name` + `type_args`; `base_name`, the
`monomorphized_struct_args` side table, `generic_type_args`'s table scan and
`fq_type_name`'s recovery step are all gone. e2e went 1444 failures to 10, all
in `reflect_*`.

Eight faults were fixed on the way, and seven were the same mistake: a bulk
rename bound `decl_name` where the site wanted the rendered spelling — the
struct name index, WIR type resolution, monomorphize's method names,
`get_type_name_info`. The WEP said a bulk rename compiles and is silently wrong;
it was written before doing exactly that twice.

The remaining 10 share one cause, and it is the design's real cost. Registration
keys the WIR struct map on `NirStruct.name` — a string the monomorphizer
produced — while the lookup now _re-derives_ the spelling from `decl_name` and
`type_args`. Those agree only while the argument `TypeId`s render the same way
at both moments, and erasure redirects mean they need not: `FlagsBit<T>` is
registered under one spelling and looked up under another, falling through to
`AbstractRef`.

That diagnosis was wrong, and measuring killed it twice. Deriving the
registration key the same way the lookup does is a **no-op** — the index it
consults is already keyed on the rendered spelling, so the derivation returns
the string it started from. And printing the map at the failure shows the real
answer: asked for `FlagsBit<u32>`, the map holds **no FlagsBit entry at all**.

So this is not a spelling mismatch. The monomorphized `FlagsBit<u32>` struct
never reaches WIR registration.

The instantiation scan does match only `GenericInstance`, so an applied `Struct`
is never queued — but making it match both changes nothing measurable, so that
is not the path either. Three proposed fixes for these ten have now been
implemented and measured, and all three were no-ops. Each was reverted rather
than kept for looking principled.

What is established, all measured: the WIR struct map holds no `FlagsBit` entry,
and yet `instantiate_struct` _does_ emit a `TirStruct` for it. So monomorphize
is doing its job and the type is lost somewhere between there and WIR
registration — DCE, link, or the flat-package assembly.

Narrowed further by reading: `dce.rs` retains a monomorphized struct only when
`struct_monomorph_names` contains its `TirStruct` name, and that set is
populated from `analysis.used_types` — so a type absent from `used_types` has
its struct dropped even though monomorphize emitted it. This refactor rewrote
how that set is filled (`base_name.is_some()` became `type_args.is_empty()`,
and the name is now derived rather than read off the type).

Measured, and it is the spelling one:

    drop "FlagsBit<reflect_flags_derive.wado/Perms>"
    have ["FlagsBit<u32>"]

The `TirStruct` is named with the flags newtype `Perms`; the reachability set
holds the same struct named with `u32`, its erased base. DCE therefore drops a
struct that is reachable.

This is the cost of a derived name, now concrete. The `TirStruct` name was fixed
at monomorphize time, before erasure. The reachability set derives its name from
the type's `type_args` _after_ erasure has redirected those ids, so the same
function renders a different string. While the name was stored on the type, both
sides read the one string and the question never arose.

### The reverse lookup a split name cannot answer

`type \`TreeMap\` does not implement trait \`IndexAssign<K>\``read as an
elaborator failure and was not one. The elaborator resolves the impl correctly —
measured at every step: the impl index holds 10 refs for`Decl((core:collections, "TreeMap"))`, the`starts_with`filter admits`IndexAssign<K>`, and the projection returns the`index_assign` method with both
gates passing. The diagnostic comes from the _WIR-build_ trait-bound check, which
reports a call left spelled with its template name.

The call never got monomorphized because `get_struct_info_from_type` answered
with no type arguments. It took the struct's stored `name` and reverse-looked it
up in `mangled_to_key` — a map from rendered spelling to `InstantiationKey`. That
worked only while the struct carried its rendering. With `decl_name` and
`type_args` split, the lookup misses, the fallthrough returns `(name, vec![])`,
and monomorphize's "receiver is an instantiated generic struct" branch is gated
on the arguments being non-empty.

The fix is to delete the reverse lookup. `mangled_to_key` recovers exactly
`(key.name, key.impl_type_args)`, and those are what `make_monomorphized_struct`
now stores as `decl_name` and `type_args` — the round trip through a string was
the only reason the map was consulted:

    ResolvedType::Struct { decl_name, type_args, .. } =>
        Some((decl_name.clone(), type_args.clone())),

This is the shape of defect the fusion was hiding. A rendered name is a lossy
encoding of a pair, and every reader that decoded it back into a pair was a
silent dependency on the encoding. `get_struct_name_from_type` — the sibling that
genuinely wants the rendering — keeps working, because it renders rather than
parses.

### The two namespaces the split exposed

Splitting the field made `decl_name` reachable at every one of the ~160 match
sites, and it reads like "the struct's name" while being the one spelling
nothing is stored under. Sixteen sites took it as such, in two shapes:

- **A registry lookup.** `struct_fields_map`, `struct_fields`, `struct_index`,
  `single_field`, `package.structs` — every struct registry is keyed by the
  _rendered_ name, because it holds one entry per instantiation. Keyed by
  `decl_name` they miss every instantiation and fall through to the
  conservative branch.
- **A minted name.** A `StructLiteral`'s `struct_name` and a `Box` wrapper's
  name must spell the instantiation, or WIR build resolves them to nothing.

One inversion was worse than a miss. `needs_copy_in_env` translated
`base_name.as_deref() != Some(box_name)` into
`!(type_args.is_empty() || decl_name == box_name)`, which is not the same
predicate: the original says "not a `Box` instantiation", the rewrite says
"is an instantiation and not `Box`". Every non-generic struct lost its deep
copy, so `let snapshot = c` aliased `c` — a silent miscompile that eleven
fixtures caught.

`TypeTable::struct_list_name` now owns this namespace: the rendered name for a
`Struct` and for a `GenericInstance` alike, documented as the key a struct list
is indexed by. It replaces `struct_decl_name`, whose two arms disagreed —
rendering the instance and not the struct — which is what made the wrong answer
look like the local convention.

### The fusion that is still there

`ResolvedType::Newtype` was not split, and it shows. Its `name` bakes the
arguments into the head (`MyArray<i32>`), so `impl_receiver_key` and
`newtype_own_name` both handed the impl index a name no `impl` header writes.
The guard that stops a newtype's own method from being retargeted at the base it
inherits from therefore never fired for a generic newtype, and
`impl MyArray<T> { fn second }` silently resolved to `List<i32>::second`, which
no template defines. Both sites now split the head; the honest fix is to give
`Newtype` the same `decl_name` / `type_args` split `Struct` has, which is
step 6.

Deriving through the unerased view was tried — spelling a newtype / flags
argument by its own declaration rather than its base — and made things _worse_:
reflect stayed at 10 and serde went 4 to 6. Reverted. So the reachability set is
not uniformly pre-erasure either; some of what it holds is spelled post-erasure
and matches definitions named that way.

Which means the retain check was comparing names minted at two different points
in the pipeline, and no single view makes both sides agree. What it needed was
not a better spelling but the same _derivation_ on both sides: DCE's analysis
renders `struct_rendered_name(decl_name, type_args)` off the resolved type, so
retention now derives that same rendering from the instantiation the struct
records in `monomorph_info`, instead of comparing the name monomorphize stored.
Both sides then read the arguments through the erased view, and the drift has
nowhere to enter.

The stored name is still there, and retention still accepts it, because a
non-monomorphized struct has no `monomorph_info` to derive from. That is the
remaining seam — closing it means carrying the struct's `TypeId` on `TirStruct`
/ `NirStruct`, which is step 7.

### An invariant the arguments broke

Splitting the arguments out gave the type table a second way to be
inconsistent. `TypeTable::retain` documents an invariant — `get(id)` never
panics for a surviving id — and closed the kept set over `redirects` to hold it.
But a monomorphized struct records its arguments as they were _before_ erasure,
and the reachability walk reaches every type through the erased view, so nothing
kept a flags argument's own id alive. The struct survived, spelling itself with
an id that no longer resolved, and rendering it after DCE panicked. While the
name was a stored string this could not happen: the string held no ids.

`retain` now closes over each surviving struct's `type_args` transitively, which
is the same reasoning that motivated the `redirects` closure — a type that
survives must be readable.

### One spelling, two functions that build it

A branch review found the same class again, in the one place the newtypes do not
reach: a reference receiver is spelled by `Receiver::mangle_with_ref` when a
definition is named and by `FqTypeName::to_mangled` when a call site looks one
up. Nothing made the two agree, and they did not.

`to_mangled` applied the receiver's arguments to a `&` head, giving
`&<List<i32>>` for what the other spells `&List<i32>`, so every ref-impl lookup
candidate matched no registered template and was silently dead. It also wrote a
space after both ref prefixes: right for `&mut X`, wrong for `&X`. A reference
is not a head with arguments — it is a pointee carrying a kind, which is what
the `reference` field already said and the rendering ignored.

The regression test asserts the two functions agree, rather than asserting
either one's output. A test that pins one spelling would have passed throughout.

DCE had the matching asymmetry a namespace type cannot catch either: definitions
keyed on `full_method_name` and call sites on `method_name`, so a method with
method type args was keyed two ways and could be collected as unreachable. Step
7 removes the class by keying on identity.

### The module a method name records twice

Regenerating the goldens — the first regeneration since this refactor began, as
its commits touched none — showed every method name carrying its module twice:

    core:prelude/string.wado/core:prelude/string.wado/String::with_capacity

`Receiver::head_key` returns a module-qualified `to_mangled`, `struct_name`
returns that, and `MangledName::in_module` prefixes the defining module again.
It is redundant, not wrong: the key is `(impl module, qualified struct, trait,
method)`, and both the definition and the lookup build it the same way, so names
stay injective and both suites pass.

Neither half is removable on its own. Drop the module prefix and a builtin
receiver loses its only qualifier — `i32^Display::fmt` names no module at all,
so two modules implementing `Display for i32` collide. Make the struct head
local instead and `impl Foo for a/T` and `impl Foo for b/T`, both written in
`c`, collide as `c/T^Foo::m`. The redundancy is what currently keeps both cases
apart, and only a key that carries the two modules as separate fields removes it
without losing either — which is step 5's `StructListKey` generalized to method
keys.

## Consequences

The compiler stops accepting the class of code this session kept producing. A
name cannot be built without saying which namespace it is in, and cannot be used
where another is expected.

Costs and risks:

- Steps 3 and 4 are large. Step 4 touches `ResolvedType::Struct`, matched in ~160
  places (nearly all with `..`), and changes interning keys.
- Step 1 is behaviour-preserving by construction: it changes only what compiles.
  It goes first for that reason. Steps 2–4 change behaviour and each needs the
  e2e suite as its gate — this session broke 45 fixtures with one such change
  that looked locally correct.
- The rendered-string escape hatches must go as the newtypes land, or callers
  will route around them. `split_base_name` on trait names and
  `display_type_name` at the WIR layer are the two that remain.

Order:

- [x] 1a. `DeclName` / `DeclPath` + the CM interface registry, the adapter map
      and `impl_target_of` keyed by them.
- [x] 1b. `MangledName`, so `head_key` and `decl_key` are no longer
      interchangeable — the swap behind three of the six defects.
- [x] 1c. `MangledName` on `func_map`, built only through `in_module` /
      `builtin_alias` / `wasi_import`. `WirName.fq` stays a `String`: it names
      types as well as functions, so the index key is the boundary.
-
  2. [x] `LocalMethodName::struct_name` field → derived method.
-
  3. [x] `MethodOwner` replacing `inherited_from_base`. Its sibling fields
         were already gone with step 2, so the fact now has one encoding.
- [x] 4a. Delete `is_monomorphized` from `ResolvedType::Struct` and
      `FreeFunctionName`. It duplicated `base_name.is_some()`, and with only two
      constructors nothing could ever make the two disagree.
- [x] 4b. `decl_name` holds the declaration and the arguments sit beside it as
      `Vec<TypeId>`, so the fused spelling is derived. `struct_list_name` owns
      the rendering; the struct-list registries are keyed by it.
-
  5. [ ] `StructListKey` as a type, so a registry cannot be keyed by
         `(decl_name, module)` at all. Sixteen readers took the split field for
         the stored name; a newtype makes that a compile error rather than a
         convention.
-
  6. [ ] Split the fused spelling out of `ResolvedType::Newtype` the way 4b did
         for `Struct`. Its `name` still bakes arguments into the head
         (`MyArray<i32>`), which is why the impl index was queried under a name it
         never registers.
-
  7. [ ] Carry the struct's `TypeId` on `TirStruct` / `NirStruct`, so DCE
         retention asks identity instead of deriving a name that has to match one
         built elsewhere.

  8. [ ] A method key holding the impl module and the receiver's module as
         separate fields, so the spelling stops carrying one module twice.
         Blocked on 5 for the same reason: the redundancy is load-bearing until
         the key is structured.
