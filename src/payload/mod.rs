//! Payload building API
//!
//! This API is used to construct individual payloads.

mod block;
mod checkpoint;
mod exec;
mod ext;
mod span;

pub use {
	block::{BlockContext, Error as BlockError},
	checkpoint::{Checkpoint, Error as CheckpointError},
	exec::{Executable, ExecutionError, ExecutionResult, IntoExecutable},
	ext::{CheckpointExt, SpanExt},
	span::{Error as SpanError, Span},
};

#[cfg(test)]
mod tests;
