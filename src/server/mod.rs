//! LSP handlers, ordered dispatch and external task lifecycle.

mod backend;
pub mod client;
mod completion;
pub mod dispatch;
mod files;
pub mod protocol;
pub(crate) mod queries;

pub use backend::Backend;

pub mod builds;
