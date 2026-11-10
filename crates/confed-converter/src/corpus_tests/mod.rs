//! Guarantees checked against the fixture corpus that reach into the
//! converter's internals — how storage splits into blocks, how Markdown does —
//! and so live inside the crate rather than in `tests/`.

mod invariants;
mod patching;
