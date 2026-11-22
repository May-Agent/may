//! Test support shared by this crate's `#[cfg(test)]` code, forwarded from
//! `may-protocol`'s `testing` feature, which this crate's
//! `[dev-dependencies]` enable.

#[allow(unused_imports)]
pub use may_protocol::test_support::{
    ApiReply, ApiRequest, ApiRequests, install_global_executor, serve_api,
};
