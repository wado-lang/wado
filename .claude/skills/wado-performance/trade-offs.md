# Performance Trade-offs

Cost-model changes that were adopted although some row got slower. Each entry
records which rows moved each way and why the change was kept anyway.

This file is a different record from `dead-ends.md`. A dead end moved nothing
and was dropped. A trade-off moved rows both ways and was kept. A later change
to the same price starts from this file: retuning the price brings the losing
row back, and a narrower rule may win both sides.

An entry names the price that changed (a constant, a predicate), the A/B for
every row whose wasm changed, the reason the losing row lost, and who decided
to keep the change.

## `param_spec`'s clone cap raised from 128 to 4096 (2026-10-28)

`MAX_TOTAL_SPECIALIZATIONS` bounds the clones `param_spec` mints over a whole
module. Clones are minted first come, first served, and a dead one still
counts. At 128, nine of the fifteen benchmarks reached the cap, so visit order
decided which calls specialized. On json_catalog, a splice that minted a few
more clones elsewhere cost the `Formatter` fold in `prepare_int_write`, and
every integer allocated a `Formatter`. gale_gen needs between 1024 and 4096.

Every Wado row ran through `mise run all-wado`, three rounds per arm, in
alternating order. Both arms carry the sole-site splice of the same day.

| Row                          | Change    | `-O2` wasm |
| ---------------------------- | --------- | ---------- |
| json_catalog / ser           | +15.7%    | +2.8%      |
| gale_gen                     | +2.2%     | +17.9%     |
| gale_gen (1 GiB heap)        | +3.0%     | +17.9%     |
| json_twitter / ser           | +1.6%     | +3.5%      |
| json_catalog / de            | +0.8%     | +2.8%      |
| cbor_canada / de             | +0.7%     | +0.2%      |
| cbor_catalog / ser           | +0.6%     | 0%         |
| cbor_canada / ser            | **−0.9%** | +0.2%      |
| cbor_twitter / ser           | **−2.5%** | 0%         |
| sqlite_parse                 | noise     | +5.3%      |
| syntax_highlight             | noise     | +5.2%      |
| sieve, fts, zlib (no change) | noise     | 0%         |

The losing CBOR rows changed no more than 0.2% in size, so a few more clones
moved code rather than adding much. gale_gen at `-O2` compiles 22% faster and
peaks 36% higher in RSS. Two narrower rules were set aside: counting only live
clones, and spending the cap on calls in loops first. The user chose the
higher cap.

## `br_table` for a long cascade at any density (2026-10-20)

`match_to_switch` turned a `match` into a `br_table` only at a density of 0.75
or more (cases / range). A generated parser's token dispatch is far below that.
In the SQLite parser's `atn_decide_168`, the dispatch has 11 arms and 16 spans
over token ids 0 to 158, a density of 0.26. It lowered to a compare cascade, and
the common tokens reached its last arms. `SWITCH_LONG_CASCADE_SPANS` now also
admits a cascade testing 12 or more spans, whatever its density.

Only the rows whose wasm changed were timed. They ran through
`mise run all-wado`, three rounds per arm, in alternating order.

| Row                            | Change    |
| ------------------------------ | --------- |
| sqlite_parse                   | +2.4%     |
| syntax_highlight               | +2.6%     |
| gale_gen                       | +1.4%     |
| gale_gen (1 GiB heap)          | +1.6%     |
| cbor_twitter / de              | +1.2%     |
| json_twitter / de              | **−2.3%** |
| json_twitter, cbor_twitter ser | noise     |
| sieve (control)                | noise     |

The loss is `FieldSchema::lookup`. Its `match key.len()` has about 20
single-value arms at a density of about 0.66, so it now crosses the span rule.
The key length changes on every call, so the indirect branch mispredicts where
the cascade did not. A rule that admits only cascades with a multi-span arm (a
token set) would leave the lookup alone, but it would be fitted to these two
cases. The user chose to keep the plain span rule.

Fuel misread this change. It fell 6.5% per SQLite parse where time fell 2.3%,
because a `br_table` counts as one instruction whatever its misprediction
costs.

## A load is not kept across a call in a cold arm (2026-10-20)

`reuse_loads` treated every call as writing every struct field. `CalleeWrites`
now gives each defined function the field names it may write, its callees
included, so a load of any other field survives the call. A call in an arm
holding `cold_path` (`is_cold_arm`) still forgets every field.

The rule is the price of keeping a value live across a call. Wasmtime saves a
callee-saved register for it on every entry to the function, whether or not
the call runs, and that register spares one reload. Disassembling `List::push`
written both ways showed the extra save and restore. Across a call that rarely
runs, the hot path pays the register and gets the reload back. `String::grow`
in `push_ascii_byte` is that shape, inlined into every serializer.

The rows whose wasm changed, each through `mise run all-wado`, three rounds per
arm in alternating order:

| Row              | Every call | Not in a cold arm |
| ---------------- | ---------- | ----------------- |
| microgpt infer   | **+7.2%**  | +1.2%             |
| zlib compress    | +2.5%      | +1.0%             |
| json_canada de   | +1.1%      | noise             |
| json_twitter de  | noise      | +0.9%             |
| json_catalog ser | **−7.4%**  | +0.7%             |
| json_catalog de  | noise      | −1.0%             |
| cbor_canada de   | −2.3%      | −1.1%             |
| json_canada ser  | −0.4%      | −0.9%             |
| json_twitter ser | noise      | −0.9%             |
| gale_gen         | noise      | −0.5%             |
| fts              | −1.4%      | −0.5%             |
| cbor_catalog de  | −1.4%      | noise             |
| syntax_highlight | −1.1%      | −1.3%             |
| sqlite_parse     | noise      | noise             |

Every row of the cold-arm rule sits within the spread a rerun gives, so the
summary buys about nothing there. It gives up microgpt infer. `Graph::value` there keeps
`self.nodes` across the cold `List::grow`, and the list is live across that call
anyway, so the reuse costs no register. A rule that keeps a load across a cold
call when its value is live there regardless would win both rows. The user
chose the cold-arm rule.
