//! Request handlers: pure functions from a [`Req`] and a read-only [`Reader`] to a [`Resp`].
//!
//! No handler touches SQL directly or knows which HTTP library is in use, which is what makes them
//! testable without a socket.
//!
//! [`Req`]: crate::watch::serve::response::Req
//! [`Resp`]: crate::watch::serve::response::Resp
//! [`Reader`]: crate::watch::store::Reader

pub mod live;
pub mod series;
pub mod status;
