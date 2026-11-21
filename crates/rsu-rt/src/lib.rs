//! Lock-free primitives and real-time helpers shared by every layer.
//! Nothing here allocates after construction.

pub mod alloc_guard;
pub mod clock;
pub mod seqlock;
pub mod spsc;
pub mod thread;
