//! Service type and utilities.
//!
//! Service are the abstraction of (leaf) services in Rama.

mod svc;
#[doc(inline)]
pub use svc::{BoxService, Service};

pub mod handler;
pub use handler::service_fn;
