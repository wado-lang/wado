# cm-catalog TODO

- [ ] `own<resource>` / `borrow<resource>` identity. A resource does travel as
  `own<r>` inside a `future` / `stream` payload
  (`wasi_tls_send_done_future.wado`), but the catalog declares no resource of
  its own to write an identity export against.
