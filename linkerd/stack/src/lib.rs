#![deny(warnings, rust_2018_idioms)]

pub mod fallback;
mod future_service;
pub mod layer;
pub mod layer_response;
pub mod make_ready;
pub mod map_response;
pub mod map_target;
pub mod new_service;
pub mod oneshot;
pub mod shared;

pub use self::fallback::{Fallback, FallbackLayer};
pub use self::future_service::FutureService;
pub use self::layer_response::{LayerResponse, LayerResponseLayer};
pub use self::map_target::{MapTarget, MapTargetLayer, MapTargetService};
pub use self::new_service::NewService;
pub use self::shared::Shared;
