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
