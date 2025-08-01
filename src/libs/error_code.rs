use serde::*;

/// `ErrorCode` is a wrapper around `u32` that represents an error code.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErrorCode {
    /// The error code (e.g. `100400` for BadRequest).
    code: u32,
}

impl ErrorCode {
    /// Indicated a bad request.
    pub const BAD_REQUEST: Self = Self::new(100400);

    /// Indicates forbidden access.
    pub const FORBIDDEN: Self = Self::new(100403);

    /// Indicates an internal server error.
    pub const INTERNAL_ERROR: Self = Self::new(100500);

    /// Indicates a non-implemented endpoint.
    pub const NOT_IMPLEMENTED: Self = Self::new(100501);

    /// Create a new `ErrorCode` from a `u32`.
    pub const fn new(code: u32) -> Self {
        Self { code }
    }

    /// Get the `ErrorCode` as a `u32`.
    /// Just returns the code field.
    pub const fn to_u32(self) -> u32 {
        self.code
    }

    /// Getter for the `ErrorCode` code field.
    pub const fn code(self) -> u32 {
        self.to_u32()
    }
}
