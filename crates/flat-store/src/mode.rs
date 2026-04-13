mod sealed {
    pub trait Sealed {}
}

/// Marker trait for the access mode of a [`FlatStore`](crate::FlatStore).
///
/// Implemented by [`RO`] (read-only) and [`RW`] (read-write).
pub trait Mode: sealed::Sealed {}

/// Read-only access mode.
pub struct RO;

/// Read-write access mode.
pub struct RW;

impl sealed::Sealed for RO {}
impl sealed::Sealed for RW {}

impl Mode for RO {}
impl Mode for RW {}
