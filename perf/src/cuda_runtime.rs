// Module for cuda-related helper functions and wrappers.
//
// cudaHostRegister/cudaHostUnregister -
//    apis for page-pinning memory. Cuda driver/hardware cannot overlap
//    copies from host memory to GPU memory unless the memory is page-pinned and
//    cannot be paged to disk. The cuda driver provides these interfaces to pin and unpin memory.

use crate::perf_libs;
use crate::recycler::Reset;
use rand::seq::SliceRandom;
use rand::Rng;
use rayon::prelude::*;
use std::ops::{Index, IndexMut};
use std::slice::SliceIndex;

use std::os::raw::c_int;

const CUDA_SUCCESS: c_int = 0;

pub fn pin<T>(_mem: &mut Vec<T>) {
    if let Some(api) = perf_libs::api() {
        unsafe {
            use core::ffi::c_void;
            use std::mem::size_of;

            let err = (api.cuda_host_register)(
                _mem.as_mut_ptr() as *mut c_void,
                _mem.capacity() * size_of::<T>(),
                0,
            );
            if err != CUDA_SUCCESS {
                panic!(
                    "cudaHostRegister error: {} ptr: {:?} bytes: {}",
                    err,
                    _mem.as_ptr(),
                    _mem.capacity() * size_of::<T>()
                );
            }
        }
    }
}

pub fn unpin<T>(_mem: *mut T) {
    if let Some(api) = perf_libs::api() {
        unsafe {
            use core::ffi::c_void;

            let err = (api.cuda_host_unregister)(_mem as *mut c_void);
            if err != CUDA_SUCCESS {
                panic!("cudaHostUnregister returned: {} ptr: {:?}", err, _mem);
            }
        }
    }
}

// A vector wrapper where the underlying memory can be
// page-pinned. Controlled by flags in case user only wants
// to pin in certain circumstances.
#[derive(Debug)]
pub struct PinnedVec<T> {
    x: Vec<T>,
    pinned: bool,
    pinnable: bool,
}

impl<T: Default + Clone> Reset for PinnedVec<T> {
    fn reset(&mut self) {
        self.resize(0, T::default());
    }

    fn warm(&mut self, size_hint: usize) {
        self.set_pinnable();
        self.resize(size_hint, T::default());
    }
}

impl<T: Clone> Default for PinnedVec<T> {
    fn default() -> Self {
        Self {
            x: Vec::new(),
            pinned: false,
            pinnable: false,
        }
    }
}

pub struct PinnedIter<'a, T>(std::slice::Iter<'a, T>);

pub struct PinnedIterMut<'a, T>(std::slice::IterMut<'a, T>);

impl<'a, T> Iterator for PinnedIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl<'a, T> Iterator for PinnedIterMut<'a, T> {
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl<'a, T> IntoIterator for &'a mut PinnedVec<T> {
    type Item = &'a T;
    type IntoIter = PinnedIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        PinnedIter(self.x.iter())
    }
}

impl<'a, T> IntoIterator for &'a PinnedVec<T> {
    type Item = &'a T;
    type IntoIter = PinnedIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        PinnedIter(self.x.iter())
    }
}

impl<T, I: SliceIndex<[T]>> Index<I> for PinnedVec<T> {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        &self.x[index]
    }
}

impl<T, I: SliceIndex<[T]>> IndexMut<I> for PinnedVec<T> {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.x[index]
    }
}

impl<T> PinnedVec<T> {
    pub fn iter(&self) -> PinnedIter<T> {
        PinnedIter(self.x.iter())
    }

    pub fn iter_mut(&mut self) -> PinnedIterMut<T> {
        PinnedIterMut(self.x.iter_mut())
    }
}

impl<'a, T: Send + Sync> IntoParallelIterator for &'a PinnedVec<T> {
    type Iter = rayon::slice::Iter<'a, T>;
    type Item = &'a T;
    fn into_par_iter(self) -> Self::Iter {
        self.x.par_iter()
    }
}

impl<T: Clone> PinnedVec<T> {
    pub fn reserve_and_pin(&mut self, size: usize) {
        if self.x.capacity() < size {
            if self.pinned {
                unpin(&mut self.x);
                self.pinned = false;
            }
            self.x.reserve(size);
        }
        self.set_pinnable();
        if !self.pinned {
            pin(&mut self.x);
            self.pinned = true;
        }
    }

    pub fn set_pinnable(&mut self) {
        self.pinnable = true;
    }

    pub fn from_vec(source: Vec<T>) -> Self {
        Self {
            x: source,
            pinned: false,
            pinnable: false,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let x = Vec::with_capacity(capacity);
        Self {
            x,
            pinned: false,
            pinnable: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.x.is_empty()
    }

    pub fn len(&self) -> usize {
        self.x.len()
    }

    pub fn as_ptr(&self) -> *const T {
        self.x.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.x.as_mut_ptr()
    }

    fn prepare_realloc(&mut self, new_size: usize) -> (*mut T, usize) {
        let old_ptr = self.x.as_mut_ptr();
        let old_capacity = self.x.capacity();
        // Predict realloc and unpin.
        if self.pinned && self.x.capacity() < new_size {
            unpin(old_ptr);
            self.pinned = false;
        }
        (old_ptr, old_capacity)
    }

    pub fn push(&mut self, x: T) {
        let (old_ptr, old_capacity) = self.prepare_realloc(self.x.len() + 1);
        self.x.push(x);
        self.check_ptr(old_ptr, old_capacity, "push");
    }

    pub fn truncate(&mut self, size: usize) {
        self.x.truncate(size);
    }

    pub fn resize(&mut self, size: usize, elem: T) {
        let (old_ptr, old_capacity) = self.prepare_realloc(size);
        self.x.resize(size, elem);
        self.check_ptr(old_ptr, old_capacity, "resize");
    }

    pub fn append(&mut self, other: &mut Vec<T>) {
        let (old_ptr, old_capacity) = self.prepare_realloc(self.x.len() + other.len());
        self.x.append(other);
        self.check_ptr(old_ptr, old_capacity, "resize");
    }

    pub fn append_pinned(&mut self, other: &mut Self) {
        let (old_ptr, old_capacity) = self.prepare_realloc(self.x.len() + other.len());
        self.x.append(&mut other.x);
        self.check_ptr(old_ptr, old_capacity, "resize");
    }

    pub fn shuffle<R: Rng>(&mut self, rng: &mut R) {
        self.x.shuffle(rng)
    }

    fn check_ptr(&mut self, _old_ptr: *mut T, _old_capacity: usize, _from: &'static str) {
        let api = perf_libs::api();
        if api.is_some()
            && self.pinnable
            && (self.x.as_ptr() != _old_ptr || self.x.capacity() != _old_capacity)
        {
            if self.pinned {
                unpin(_old_ptr);
            }

            trace!(
                "pinning from check_ptr old: {} size: {} from: {}",
                _old_capacity,
                self.x.capacity(),
                _from
            );
            pin(&mut self.x);
            self.pinned = true;
        }
    }
}

impl<T: Clone> Clone for PinnedVec<T> {
    fn clone(&self) -> Self {
        let mut x = self.x.clone();
        let pinned = if self.pinned {
            pin(&mut x);
            true
        } else {
            false
        };
        debug!(
            "clone PinnedVec: size: {} pinned?: {} pinnable?: {}",
            self.x.capacity(),
            self.pinned,
            self.pinnable
        );
        Self {
            x,
            pinned,
            pinnable: self.pinnable,
        }
    }
}

impl<T> Drop for PinnedVec<T> {
    fn drop(&mut self) {
        if self.pinned {
            unpin(self.x.as_mut_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pinned_vec() {
        let mut mem = PinnedVec::with_capacity(10);
        mem.set_pinnable();
        mem.push(50);
        mem.resize(2, 10);
        assert_eq!(mem[0], 50);
        assert_eq!(mem[1], 10);
        assert_eq!(mem.len(), 2);
        assert_eq!(mem.is_empty(), false);
        let mut iter = mem.iter();
        assert_eq!(*iter.next().unwrap(), 50);
        assert_eq!(*iter.next().unwrap(), 10);
        assert_eq!(iter.next(), None);
    }
}
