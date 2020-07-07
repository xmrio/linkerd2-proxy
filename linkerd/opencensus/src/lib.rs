#![deny(warnings, rust_2018_idioms)]
use futures::{Stream, StreamExt};
use http_body::Body as HttpBody;
use linkerd2_error::Error;
use metrics::Registry;
pub use opencensus_proto as proto;
use opencensus_proto::agent::common::v1::Node;
use opencensus_proto::agent::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use opencensus_proto::trace::v1::Span;
use std::convert::TryInto;
use std::mem;
use tokio::sync::mpsc;
use tonic::{
    self as grpc,
    body::{Body as GrpcBody, BoxBody},
    client::GrpcService,
};
use tracing::{debug, trace};

pub mod metrics;

const MAX_BATCH_SIZE: usize = 100;

pub async fn export_spans<T, S>(client: T, node: Node, spans: S, mut metrics: Registry)
where
    T: GrpcService<BoxBody> + Send + 'static,
    S: Stream<Item = Span> + Unpin,
    T::Error: Into<Error> + Send,
    T::ResponseBody: Send + 'static,
    <T::ResponseBody as GrpcBody>::Data: Send,
    <T::ResponseBody as HttpBody>::Error: Into<Error> + Send,
{
    let mut node = Some(node);
    let mut svc = TraceServiceClient::new(client);
    tokio::pin!(spans);
    loop {
        let (mut request_tx, request_rx) = mpsc::channel(1);
        let req = grpc::Request::new(request_rx);
        trace!("Establishing new TraceService::export request");
        let mut rsp = Box::pin(svc.export(req));
        metrics.start_stream();
        let mut batch = Vec::new();
        'stream: loop {
            tokio::select! {
                done = &mut rsp => {
                    match done {
                        Ok(_) => trace!("response future completed, restarting RPC..."),
                        Err(error) => debug!(%error, "response future failed, restarting RPC..."),
                    };
                    break 'stream;
                }
                next = spans.next() => match next {
                    Some(span) => {
                        batch.push(span);
                        if batch.len() == MAX_BATCH_SIZE {
                            if send_spans(mem::replace(&mut batch, Vec::new()), &mut request_tx, &mut node, &mut metrics).await.is_err() {
                                // The sender went away unexpectedly!
                                break 'stream;
                            }
                        }
                    },
                    None => {
                        tracing::trace!("span stream completed, flushing...");
                        let _ =send_spans(batch, &mut request_tx, &mut node, &mut metrics).await;
                        return;
                    }
                }
            }
        }
    }
}

async fn send_spans(
    spans: Vec<Span>,
    sender: &mut mpsc::Sender<ExportTraceServiceRequest>,
    node: &mut Option<Node>,
    metrics: &mut Registry,
) -> Result<(), ()> {
    if spans.is_empty() {
        return Ok(());
    }

    if let Ok(num_spans) = spans.len().try_into() {
        metrics.send(num_spans);
    }
    let req = ExportTraceServiceRequest {
        spans,
        node: node.take(),
        resource: None,
    };
    trace!(message = "Transmitting", ?req);
    sender.send(req).await.map_err(|_| ())
}
