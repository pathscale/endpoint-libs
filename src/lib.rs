#![cfg_attr(test, allow(deprecated))]
#![cfg_attr(not(feature = "std-deps"), no_std)]

#[cfg(feature = "frame-core")]
extern crate alloc;

pub mod libs;
#[cfg(feature = "types")]
pub mod model;
