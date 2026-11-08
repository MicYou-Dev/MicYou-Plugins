//! Single-producer / single-consumer lock-free ring buffer.
//!
//! Producer: the real-time audio thread. Consumer: the writer thread. No
//! allocation, locking or syscalls happen on the producer side.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct SpscRing {
    buf: Box<[UnsafeCell<f32>]>,
    mask: usize,
    head: AtomicUsize, // written by producer
    tail: AtomicUsize, // written by consumer
}

// The producer and consumer touch disjoint indices; `UnsafeCell` is sound here
// because only the producer writes slots before publishing `head` and only the
// consumer reads them after observing `head`.
unsafe impl Sync for SpscRing {}
unsafe impl Send for SpscRing {}

impl SpscRing {
    /// `capacity` is rounded up to the next power of two.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        let mut buf = Vec::with_capacity(cap);
        buf.resize_with(cap, || UnsafeCell::new(0.0f32));
        Self {
            buf: buf.into_boxed_slice(),
            mask: cap - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Push as many samples as fit. Returns the number of samples dropped.
    pub fn push_slice(&self, src: &[f32]) -> usize {
        if src.is_empty() {
            return 0;
        }
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let used = head.wrapping_sub(tail);
        let free = self.buf.len().saturating_sub(used);
        let count = src.len().min(free);

        for (i, sample) in src[..count].iter().enumerate() {
            let slot = (head.wrapping_add(i)) & self.mask;
            unsafe {
                *self.buf[slot].get() = *sample;
            }
        }
        if count > 0 {
            self.head.store(head.wrapping_add(count), Ordering::Release);
        }
        src.len() - count
    }

    /// Append all currently available samples to `dst`. Called by one consumer
    /// only (the writer thread).
    pub fn pop_into(&self, dst: &mut Vec<f32>) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let available = head.wrapping_sub(tail).min(self.buf.len());
        if available == 0 {
            return 0;
        }
        dst.reserve(available);
        for i in 0..available {
            let slot = (tail.wrapping_add(i)) & self.mask;
            dst.push(unsafe { *self.buf[slot].get() });
        }
        self.tail.store(head, Ordering::Release);
        available
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }

    pub fn clear(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(head, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_overflow() {
        let ring = SpscRing::new(8);
        assert_eq!(ring.push_slice(&[1.0, 2.0, 3.0]), 0);
        let mut out = Vec::new();
        assert_eq!(ring.pop_into(&mut out), 3);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);

        assert_eq!(ring.push_slice(&[0.0; 10]), 2);
        out.clear();
        assert_eq!(ring.pop_into(&mut out), 8);
        assert!(ring.is_empty());
    }
}
