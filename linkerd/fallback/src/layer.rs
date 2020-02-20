use linkerd2_error::Error;

/// A fallback layer composing two service builders.
///
/// If the future returned by the primary builder's `MakeService` fails with
/// an error matching a given predicate, the fallback future will attempt
/// to call the secondary `MakeService`.
#[derive(Clone, Debug)]
pub struct FallbackLayer<F, P = fn(&Error) -> bool> {
    fallback: F,
    predicate: P,
}

// === impl FallbackLayer ===

impl<B> FallbackLayer<B> {
    pub fn new(fallback: B) -> Self {
        let predicate: fn(&Error) -> bool = |_| true;
        Self {
            fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that uses the given `predicate` to determine whether
    /// to fall back.
    pub fn with_predicate<P>(self, predicate: P) -> FallbackLayer<B, P>
    where
        P: Fn(&Error) -> bool + Clone,
    {
        FallbackLayer {
            fallback: self.fallback,
            predicate,
        }
    }

    /// Returns a `Layer` that falls back if the error or its source is of
    /// type `E`.
    pub fn on_error<E>(self) -> FallbackLayer<B>
    where
        E: std::error::Error + 'static,
    {
        self.with_predicate(|e| e.is::<E>() || e.source().map(|s| s.is::<E>()).unwrap_or(false))
    }
}

impl<A, B, P> tower::layer::Layer<A> for FallbackLayer<B, P>
where
    B: Clone,
    P: Clone,
{
    type Service = super::Fallback<A, B, P>;

    fn layer(&self, inner: A) -> Self::Service {
        Self::Service {
            inner,
            fallback: self.fallback.clone(),
            predicate: self.predicate.clone(),
        }
    }
}
