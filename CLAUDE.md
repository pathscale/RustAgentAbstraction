@AGENTS.md

# Claude Code notes: RustAgentAbstraction

The import above is binding: [`AGENTS.md`](AGENTS.md) is the **working agreement** for this
repository, and every Claude Code session loads it automatically. Don't copy rules here,
since one source of truth means no drift. Only genuinely Claude-specific wiring belongs
below.

- This crate drives `claude` itself. When you change the Claude adapter in
  [`src/agent.rs`](src/agent.rs), you are changing how a program invokes the same CLI you
  are running inside. Verify against `claude --help` for the installed version rather than
  against your own knowledge of the flags, which may be from a different release.
- The live test suite spawns real agents and spends real quota. Don't run it on a loop, and
  don't add it to a watch task.
