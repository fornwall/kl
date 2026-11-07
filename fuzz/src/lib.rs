//! kl's own source files, which the targets in `fuzz_targets` test. kl has no
//! library, so this pulls them in the way `tests/e2e.rs` does.

#[path = "../../src/command.rs"]
pub mod command;
#[path = "../../src/ghostty/mod.rs"]
pub mod ghostty;
#[path = "../../src/input.rs"]
pub mod input;
#[path = "../../src/proto.rs"]
pub mod proto;
#[path = "../../src/sys.rs"]
pub mod sys;
