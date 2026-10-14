//! # manarouter-relay — the data plane
//!
//! This process does four things: accept a request, look up a route the control plane already
//! prepared, select an account, and shuttle bytes. **It evaluates no policy.**
//!
//! ## Why the allocator lives here rather than in the binary
//!
//! Manarouter's central claim is that the relay adds less than a millisecond, and
//! `tools/bench` measures that. The measurement is only meaningful if the benchmark links the same
//! allocator the shipping binary uses, and a `#[global_allocator]` declared in `main.rs` is not
//! visible to an integration test or a benchmark that links the library. Declaring it here means
//! the thing that is measured and the thing that is shipped are the same build.
//!
//! mimalloc is chosen over the system allocator because the relay's allocation pattern is
//! pathological for a general-purpose allocator in a specific way: many short-lived buffers on many
//! threads, freed by a *different* thread from the one that allocated them when a hyper task
//! migrates between workers. glibc's malloc handles that with per-arena locks; mimalloc handles it
//! with thread-local free lists and deferred reclamation, which is materially cheaper under exactly
//! this pattern.
//!
//! ## Module map
//!
//! * [`model_scan`] — finds the `model` field without parsing the JSON. On the critical path of
//!   every request.
//! * [`upstream`] — the HTTP client: pooling, the separate first-byte timeout, and streaming
//!   passthrough.
//! * [`state`] — the hot-swappable routing table, the client keys, and the one piece of mutable
//!   routing state in the process.
//! * [`select`] — step and account selection and the hedged-race policy. Pure functions, so the
//!   decisions that are most likely to be wrong can be tested without a network.
//! * [`server`] — the route table and the accept loop.
//!
//! See `docs/01-architecture.md` for the intended shape and `docs/16-latency-engineering.md` for
//! what each module is allowed to cost.

use mimalloc::MiMalloc;

/// The global allocator.
///
/// See the module documentation for why this is declared in the library rather than the binary.
#[global_allocator]
static GLOBAL_ALLOCATOR: MiMalloc = MiMalloc;

pub mod model_scan;
pub mod select;
pub mod server;
pub mod state;
pub mod sticky;
pub mod upstream;
