mod dispatch;
pub mod error;
mod layer;
mod service;

pub use self::{dispatch::Dispatch, layer::SpawnProbeBufferLayer, service::ProbeBuffer};

struct InFlight<Req, Rsp> {
    request: Req,
    tx: tokio::sync::oneshot::Sender<Result<Rsp, linkerd2_error::Error>>,
}
