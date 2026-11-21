//! Passing shared buffers to the real-time thread.
//!
//! The command queue only carries `Copy` values, and the audio thread must
//! never free memory. So an `Arc` crosses as a raw pointer: the sender
//! gives up one reference, the mixer takes it over, and whatever buffer it
//! replaces travels back the same way to be dropped on a normal thread.

use std::sync::Arc;

/// One reference to an `Arc<T>`, in a form that is `Copy`.
///
/// It is not counted while in this form: every `Handoff` must be turned
/// back into an `Arc` exactly once, or the buffer leaks.
pub struct Handoff<T: ?Sized>(*const T);

impl<T: ?Sized> Clone for Handoff<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized> Copy for Handoff<T> {}

// SAFETY: this is an `Arc<T>` in disguise and may go wherever one may.
unsafe impl<T: ?Sized + Send + Sync> Send for Handoff<T> {}

impl<T: ?Sized> Handoff<T> {
    pub fn new(arc: Arc<T>) -> Self {
        Handoff(Arc::into_raw(arc))
    }

    /// # Safety
    /// Must be called exactly once per `Handoff::new`, copies included.
    pub unsafe fn into_arc(self) -> Arc<T> {
        Arc::from_raw(self.0)
    }
}

impl<T: ?Sized> std::fmt::Debug for Handoff<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handoff")
    }
}

/// A buffer the mixer no longer uses, on its way back to be freed.
#[derive(Clone, Copy, Debug)]
pub enum Retired {
    Music(Handoff<crate::stream::Stream>),
    Samples(Handoff<[Arc<[f32]>]>),
}

impl Retired {
    /// Drop the buffer. Call from a thread that may free memory.
    pub fn release(self) {
        // SAFETY: the mixer hands each retired buffer over exactly once.
        unsafe {
            match self {
                Retired::Music(h) => drop(h.into_arc()),
                Retired::Samples(h) => drop(h.into_arc()),
            }
        }
    }
}
