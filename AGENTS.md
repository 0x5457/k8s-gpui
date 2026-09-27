# AGENTS.md

## Testing

- Write as few unit tests as possible. Only add a unit test when it is genuinely necessary.
- A unit test is necessary only when it verifies non-obvious logic, an invariant that can silently break, or a bug fix that would otherwise regress.
- Do not add tests that restate the implementation, cover trivial getters/setters, or merely exercise the type system.
- Never use test coverage percentage as a goal.
- Do not modify existing tests to make them pass, and do not delete failing tests to make a change look green. Fix the code.

## Libraries: never reinvent the wheel

- Before writing any non-trivial helper, utility, parser, serializer, or abstraction, first check whether a mature, well-maintained library already solves the problem.
- Search the ecosystem in this order: the standard library, then existing dependencies in `Cargo.toml` / `Cargo.lock`, then well-known crates.
- If a library fits, use it. Do not reimplement what a dependency already provides, even if the reimplementation would be shorter.
- Do not add a new dependency without a clear reason, but never choose a worse design just to avoid a dependency. Explain the tradeoff instead.
- If you decide a hand-rolled implementation is genuinely better, state why in the commit message or a code comment near the code.
