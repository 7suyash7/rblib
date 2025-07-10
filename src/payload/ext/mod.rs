//! Extensions for payload building primitives.
//!
//! This module contains helper traits that extend the functionality of
//! `Checkpoint`, `Span`, and other payload building primitives with helper
//! methods that improve the developer experience and reduce boilerplate code
//! but are not strictly necessary for the core functionality.

mod checkpoint;
mod span;

pub use {checkpoint::CheckpointExt, span::SpanExt};

mod sealed {
	/// This pattern is used to prevent external implementations of the extension
	/// traits defined in this module.
	pub trait Sealed {}

	impl<P: crate::Platform> Sealed for super::super::Span<P> {}
	impl<P: crate::Platform> Sealed for super::super::Checkpoint<P> {}
}
