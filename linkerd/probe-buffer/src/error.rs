use linkerd2_error::Error;
use std::sync::Arc;

#[derive(Copy, Clone, Debug)]
pub struct Closed(pub(crate) ());

#[derive(Clone, Debug)]
pub struct Failed(pub(crate) Arc<Error>);

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Closed")
    }
}

impl std::error::Error for Closed {}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.as_ref().fmt(f)
    }
}

impl std::error::Error for Failed {}
