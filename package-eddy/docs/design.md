# Eddy Design Notes

Decisions taken so far, and the leanings not yet settled. The evidence behind
them is in [the research](./research-code-review-bots.md).

## Decisions

- [x] A reproduction agent is part of the pipeline. A severe finding is tried
  in a local environment before it is posted, and the post says whether it
  reproduced.
- [x] No agent calls `core:process`. Every capability an agent has reaches it as
  a tool or an MCP server, because current agentic models are trained to
  act through tool calls.

## Leanings

- [ ] Show `AGENTS.md` to the reproduction agent only. It holds how to build,
  test and run the project, which reproduction needs; to a reviewer most of
  it is noise that it would follow literally.
