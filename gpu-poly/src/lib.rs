#![feature(test, allocator_api, const_try, int_log, int_roundings)]

use allocator::PageAlignedAllocator;
use ark_ff::FftField;
use ark_ff_optimized::fp64;

pub mod allocator;
pub mod plan;
pub mod prelude;
pub mod stage;
pub mod utils;

// GPU implementation of the field exists in metal/
pub trait GpuField: FftField {
    // Used to select which GPU kernel to call.
    fn field_name() -> String;
}

impl GpuField for fp64::Fp {
    fn field_name() -> String {
        "fp18446744069414584321".to_owned()
    }
}

/// Shared vec between GPU and CPU.
/// Requirement is that the vec's memory is page aligned.
pub type GpuVec<T> = Vec<T, PageAlignedAllocator>;