# AGENTS.md

## Building

Builds go through [mr boxington](https://mr-boxington.jdx.dev) (`mbx`). Two rules,
both mandatory, and they exist because several agents build this workspace at
the same time:

1. **Never invoke `cargo` directly.** Write `mbx <cargo-command>`, even in a
   shell where `cargo` happens to be a wrapper. Plain `cargo` bypasses mbx's
   machine-wide CPU and memory budget, so it starves whatever else is compiling.
2. **Always give your build its own `CARGO_TARGET_DIR`.** Cargo takes an
   exclusive lock on a target directory and mbx does not lift it, so a shared
   target directory means the second command just prints
   `Blocking waiting for file lock on build directory` and hangs until the first
   one finishes. Pick a directory named after what you are doing, not
   `target/`, and do not reuse another agent's:

   ```sh
   CARGO_TARGET_DIR=target/check mbx check --workspace --all-targets
   CARGO_TARGET_DIR=target/clippy mbx clippy --workspace --all-targets -- -D warnings
   ```

   mbx then shares one machine-wide CPU and memory budget across concurrent
   commands and deduplicates identical compilations, so parallel builds neither
   thrash memory nor recompile the same unit twice. This needs no git worktree,
   only distinct target directories.

Other notes:

- Subcommands are `mbx build`, `mbx check`, `mbx clippy`, `mbx test`, and
  `mbx nextest run`. A command mbx does not cache (`cargo fmt`, unknown
  subcommands) passes straight through, so do not special-case it, and it needs
  no target directory of its own.
- A target directory you set yourself is outside mbx's pruning, so its
  subdirectories above accumulate; remove them when they are stale. The default
  `target/` is managed and pruned to a disk budget, so a large `target/` is
  expected. Do not delete either to "clean up" without being asked.
- Diagnose a surprising build with `mbx doctor`, `mbx explain --last`, and
  `mbx stats`. Use `mbx tui` to watch a running build. `mbx gc --dry-run`
  previews cleanup.

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
