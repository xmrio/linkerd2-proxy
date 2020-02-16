pub use linkerd2_error::Error;
use std::sync::Arc;

#[derive(Debug)]
pub struct Poisoned(pub(crate) ());

#[derive(Debug)]
pub struct ServiceError(pub(crate) Arc<Error>);

// === impl POisoned ===

impl std::fmt::Display for Poisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "poisoned")
    }
}

impl std::error::Error for Poisoned {}

// === impl ServiceError ===

impl ServiceError {
    pub fn inner(&self) -> &Error {
        self.0.as_ref()
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ServiceError {}
