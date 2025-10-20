use {
	super::*,
	alloy::eips::{Encodable2718, eip2718::WithEncoded},
};

/// Extension trait for [`types::Bundle`] that improves Dev Ex when
/// working with platform-agnostic bundles.
pub trait BundleExt<P: Platform>: Bundle<P> {
	/// Returns an iterator over all transactions that are allowed to fail while
	/// keeping the bundle valid.
	fn failable_txs(
		&self,
	) -> impl Iterator<Item = &Recovered<types::Transaction<P>>> {
		self
			.transactions()
			.iter()
			.filter(|tx| !self.is_allowed_to_fail(tx.tx_hash()))
	}

	/// Returns an iterator over all transactions that can be removed from the
	/// bundle without affecting the bundle validity.
	fn optional_txs(
		&self,
	) -> impl Iterator<Item = &Recovered<types::Transaction<P>>> {
		self
			.transactions()
			.iter()
			.filter(|tx| self.is_optional(tx.tx_hash()))
	}

	/// Returns an iterator over all transactions that are required to be present
	/// for the bundle to be valid. This includes also transactions that may be
	/// allowed to fail but must be present.
	fn required_txs(
		&self,
	) -> impl Iterator<Item = &Recovered<types::Transaction<P>>> {
		self
			.transactions()
			.iter()
			.filter(|tx| !self.is_optional(tx.tx_hash()))
	}

	/// Returns an iterator over all transactions that must be included and may
	/// not fail for the bundle to be valid.
	fn critical_txs(
		&self,
	) -> impl Iterator<Item = &Recovered<types::Transaction<P>>> {
		self.transactions().iter().filter(|tx| {
			!self.is_allowed_to_fail(tx.tx_hash()) && !self.is_optional(tx.tx_hash())
		})
	}

	/// Returns an iterator that yields the bundle's transactions in a format
	/// ready for execution.
	///
	/// By default, this wraps the plain `Recovered` transactions. Implementors
	/// that store pre-encoded bytes can override this to provide the more
	/// efficient `WithEncoded` wrapper.
	fn transactions_encoded(
		&self,
	) -> impl Iterator<Item = WithEncoded<&Recovered<types::Transaction<P>>>> {
		Box::new(
			self
				.transactions()
				.iter()
				.map(|tx| WithEncoded::new(tx.encoded_2718().into(), tx)),
		)
	}
}
