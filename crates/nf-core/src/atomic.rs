//! Lock-free scalar cells shared between the control or UI threads and the audio thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// An `f32` stored as its bits in an [`AtomicU32`]. Relaxed ordering: each value stands alone.
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub const fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    #[inline]
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// An `f64` stored as its bits in an [`AtomicU64`].
#[derive(Debug, Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub const fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    #[inline]
    pub fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
    #[inline]
    pub fn store(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}
