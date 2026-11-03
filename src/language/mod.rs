//! Language features and their request-specific caches and resolvers.

pub(crate) mod argument_cache;
pub(crate) mod call_hierarchy;
pub mod code_lens;
pub mod colors;
pub(crate) mod command_index;
pub(crate) mod completion_cache;
pub(crate) mod completion_resolve;
pub(crate) mod display;
pub mod features;
pub(crate) mod folding;
pub(crate) mod math_context;
pub mod navigation;
pub(crate) mod path_cache;
pub mod path_completion;
pub mod semantic;
pub(crate) mod semantic_cache;
pub mod signature;
pub(crate) mod structure;
pub(crate) mod symbol_cache;
