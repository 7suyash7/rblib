use {
	crate::*,
	alloy::{consensus::crypto::RecoveryError, primitives::TxHash},
	reth::{
		errors::ProviderError,
		ethereum::primitives::SignedTransaction,
		evm::{
			ConfigureEvm,
			Evm,
			revm::{
				DatabaseCommit,
				DatabaseRef,
				context::result::ExecResultAndState,
			},
		},
		primitives::Recovered,
		revm::{
			State,
			db::{
				BundleState,
				WrapDatabaseRef,
				states::bundle_state::BundleRetention,
			},
		},
		transaction_pool::PoolTransaction,
	},
	std::fmt::Debug,
};

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError<P: Platform> {
	#[error("Invalid signature: {0}")]
	InvalidSignature(#[from] RecoveryError),

	#[error("Invalid transaction: {0}")]
	InvalidTransaction(types::EvmError<P, ProviderError>),

	#[error("Invalid transaction {0} cannot be dropped from bundle: {1}")]
	InvalidBundleTransaction(TxHash, types::EvmError<P, ProviderError>),

	#[error("Transaction {0} in the bundle is not allowed to revert.")]
	BundleTransactionReverted(TxHash),

	#[error("Invalid bundle post-execution state: {0}")]
	InvalidBundlePostExecutionState(types::BundlePostExecutionError<P>),

	#[error("Bundle is not eligible for execution in this block")]
	IneligibleBundle,
}

/// Describes an atomic unit of execution that can be used to create a state
/// transition checkpoint.
#[derive(Debug, Clone)]
pub enum Executable<P: Platform> {
	// Individual transaction
	Transaction(Recovered<types::Transaction<P>>),

	// A bundle of transactions with metadata and behaviors.
	Bundle(types::Bundle<P>),
}

impl<P: Platform> Executable<P> {
	/// Executes this executable as a single unit of state transition and returns
	/// the outcome of the execution along with all state changes. If the
	/// executable is invalid, no execution result will be produced.
	///
	/// For details on what makes an executable invalid see the
	/// [`execute_transaction`] and [`execute_bundle`] methods.
	pub fn execute<DB>(
		self,
		block: &BlockContext<P>,
		db: &DB,
	) -> Result<ExecutionResult<P>, ExecutionError<P>>
	where
		DB: DatabaseRef<Error = ProviderError> + Debug,
	{
		match self {
			Self::Bundle(bundle) => Self::execute_bundle(bundle, block, db),
			Self::Transaction(tx) => Self::execute_transaction(tx, block, db)
				.map_err(ExecutionError::InvalidTransaction),
		}
	}

	/// Executes a single transactions and returns the ourcome of the transaction
	/// execution along with all state changes. This output is used to create a
	/// state checkpoint.
	///
	/// Notes:
	/// - Transactions that are invalid and cause EVM failures will not produce an
	///   execution result.
	///
	/// - Transactions that fail gracefully (revert or halt) will produce an
	///   execution result and state changes. It is up to higher levels of the
	///   system to decide what to do with such transactions, e.g. whether to
	///   remove them from the payload or not (see [`RevertProtection`]).
	fn execute_transaction<DB>(
		tx: Recovered<types::Transaction<P>>,
		block: &BlockContext<P>,
		db: &DB,
	) -> Result<ExecutionResult<P>, types::EvmError<P, ProviderError>>
	where
		DB: DatabaseRef<Error = ProviderError> + Debug,
	{
		let mut state = State::builder()
			.with_database(WrapDatabaseRef(db))
			.with_bundle_update()
			.build();

		let result = block
			.evm_config()
			.evm_with_env(&mut state, block.evm_env().clone())
			.transact_commit(&tx)?;

		state.merge_transitions(BundleRetention::Reverts);

		Ok(ExecutionResult {
			source: Executable::Transaction(tx),
			results: vec![result],
			state: state.take_bundle(),
		})
	}

	/// Executes a bundle of transactions and returns the outcome of the execution
	/// of all transactions in the bundle along with the aggregate of all state
	/// changes.
	///
	/// Notes:
	/// - Bundles that are not eligible for execution in the current block are
	///   considered invalid and no execution result will be produced.
	///
	/// - All transactions in the bundle are executed in the order in which they
	///   were defined in the bundle.
	///
	/// - Each transaction is executed on the state that was produced by the
	///   previous transaction in the bundle.
	///
	/// - First transaction in the bundle is executed on the state of the
	///   checkpoint that we are building on.
	///
	/// - Transactions that cause EVM errors will invalidate the whole bundle and
	///   no execution result will be produced (similar behavior to invalid loose
	///   txs),
	///     - Unless the transaction that is failing is optional
	///       [`Bundle::is_optional`]. In that case a new version of the bundle
	///       will be created by removing the invalid failing transaction through
	///       [`Bundle::without_transaction`].
	///     - If removing the invalid optional transaction results in an empty
	///       bundle, the bundle will be considered invalid and no execution
	///       result will be produced.
	///
	/// - If a transaction in the bundle fails gracefully (revert or halt):
	///     - If the bundle allows this tx to fail [`Bundle::is_allowed_to_fail`],
	///       the bundle will be considered valid and the execution result will be
	///       produced including this tx. State changes from the failed
	///       transaction will be included in the aggregate state, e.g gas used,
	///       nonces incremented, etc. Cleaning up transactions that are allowed
	///       to fail and are optional from a bundle is beyond the scope of this
	///       method. This is implemented by higher levels of the system, such as
	///       the [`RevertProtection`] step in the pipelines API.
	///     - If the bundle does not allow this tx to fail, but the transaction is
	///       optional, then it will be removed from the bundle.
	///     - If the bundle does not allow this tx to fail and the transaction is
	///       not optional, then the bundle will be considered invalid and no
	///       execution result will be produced.
	///
	/// - At the end of the bundle execution, the bundle implementation will have
	///   a chance to validate any other platform-specific post-execution
	///   requirements. For example, the bundle may require that the state after
	///   the execution has a certain balance in some account, etc. If this check
	///   fails, the bundle will be considered invalid and no execution result
	///   will be produced.
	fn execute_bundle<DB>(
		bundle: types::Bundle<P>,
		block: &BlockContext<P>,
		db: &DB,
	) -> Result<ExecutionResult<P>, ExecutionError<P>>
	where
		DB: DatabaseRef<Error = ProviderError> + Debug,
	{
		if !bundle.is_eligible(block) {
			return Err(ExecutionError::IneligibleBundle);
		}

		let evm_env = block.evm_env();
		let evm_config = block.evm_config();
		let mut db = State::builder()
			.with_database(WrapDatabaseRef(db))
			.with_bundle_update()
			.build();

		let mut discarded = Vec::new();
		let mut results = Vec::with_capacity(bundle.transactions().len());

		for transaction in bundle.transactions() {
			let result = evm_config
				.evm_with_env(&mut db, evm_env.clone())
				.transact(transaction);

			match result {
				// valid transaction
				Ok(ExecResultAndState { result, state }) => {
					if !result.is_success() {
						match (
							bundle.is_allowed_to_fail(*transaction.tx_hash()),
							bundle.is_optional(*transaction.tx_hash()),
						) {
							// transaction is not allowed to fail, but it is optional,
							// we can remove it from the bundle and continue executing the
							// bundle.
							(false, true) => {
								discarded.push(*transaction.tx_hash());
								continue;
							}

							// transaction is not allowed to fail and is not optional,
							// this invalidates the whole bundle and we cannot produce an
							// execution result.
							(false, false) => {
								return Err(ExecutionError::BundleTransactionReverted(
									*transaction.tx_hash(),
								));
							}
							// transaction is allowed to fail, include it
							_ => {}
						}
					}

					// transaction will be included
					results.push(result);
					db.commit(state);
				}
				// Invalid transaction
				Err(err) => {
					if bundle.is_optional(*transaction.tx_hash()) {
						// If the transaction is optional, we can skip it
						// and continue with the next transaction.
						discarded.push(*transaction.tx_hash());
						continue;
					}

					return Err(ExecutionError::InvalidBundleTransaction(
						*transaction.tx_hash(),
						err,
					));
				}
			}
		}

		// produce a new bundle without the discarded transactions
		let mut bundle = bundle;
		for tx in discarded {
			bundle = bundle.without_transaction(tx);
		}

		// extract all the state changes that were made by executing
		// transactions in this bundle.
		db.merge_transitions(BundleRetention::Reverts);
		let state = db.take_bundle();

		// run the optional post-execution validation of the bundle.
		bundle
			.validate_post_execution(&state)
			.map_err(ExecutionError::InvalidBundlePostExecutionState)?;

		Ok(ExecutionResult {
			source: Executable::Bundle(bundle),
			results,
			state,
		})
	}
}

impl<P: Platform> Executable<P> {
	/// Returns all transactions that make up this executable.
	pub fn transactions(&self) -> &[Recovered<types::Transaction<P>>] {
		match self {
			Self::Transaction(tx) => std::slice::from_ref(tx),
			Self::Bundle(bundle) => bundle.transactions(),
		}
	}
}

/// Convinience trait that allows all types that can be executed to be used as a
/// parameter to the `Checkpoint::apply` method.
pub trait IntoExecutable<P: Platform, S = ()> {
	fn try_into_executable(self) -> Result<Executable<P>, RecoveryError>;
}

/// Transactions can be converted into an executable as long as they have a
/// valid recoverable signature.
impl<P: Platform> IntoExecutable<P, ()> for types::Transaction<P> {
	fn try_into_executable(self) -> Result<Executable<P>, RecoveryError> {
		SignedTransaction::try_into_recovered(self)
			.map(Executable::Transaction)
			.map_err(|_| RecoveryError::new())
	}
}

/// Transactions from the transaction pool can be converted infalliably into
/// an executable because the transaction pool discards transactions
/// that have invalid signatures.
impl<P: Platform> IntoExecutable<P, u32> for types::PooledTransaction<P> {
	fn try_into_executable(self) -> Result<Executable<P>, RecoveryError> {
		Ok(Executable::Transaction(self.into_consensus()))
	}
}

/// Signature recovered individual transactions are always infalliably
/// convertable into an executable.
impl<P: Platform> IntoExecutable<P, u8> for Recovered<types::Transaction<P>> {
	fn try_into_executable(self) -> Result<Executable<P>, RecoveryError> {
		Ok(Executable::Transaction(self))
	}
}

/// Bundles are also convertible into an executable infalliably.
/// Signature recovery is part of the bundle assembly logic.
impl<P: Platform> IntoExecutable<P, u16> for types::Bundle<P> {
	fn try_into_executable(self) -> Result<Executable<P>, RecoveryError> {
		Ok(Executable::Bundle(self))
	}
}

/// This trait represents the overall result of executing a transaction or a
/// bundle of transactions.
///
/// Types implementing this trait provide access to the individual results of
/// transaction executions that make up this overall result.
#[derive(Debug, Clone)]
pub struct ExecutionResult<P: Platform> {
	source: Executable<P>,

	/// For transactions this is guaranteed to be a single-element vector,
	/// for bundles this is guaranteed to be a vector of results for each
	/// transaction in the bundle.
	results: Vec<types::TransactionExecutionResult<P>>,

	/// The aggregated state executing all transactions from the source.
	state: BundleState,
}

impl<P: Platform> ExecutionResult<P> {
	/// Returns the executable that was executed to produce this result.
	pub const fn source(&self) -> &Executable<P> {
		&self.source
	}

	/// Returns the aggregate state changes that were made by executing
	/// the transactions in this execution unit.
	pub const fn state(&self) -> &BundleState {
		&self.state
	}

	/// Access to the individual transaction results that make up this execution
	/// result.
	///
	/// For transactions, this will return a single-element slice containing the
	/// transaction's execution result. For bundles, this will return a slice of
	/// execution results for each transaction in the bundle.
	pub const fn results(&self) -> &[types::TransactionExecutionResult<P>] {
		self.results.as_slice()
	}

	/// Returns individual transactions that were executed as part of this
	/// execution unit.
	pub fn transactions(&self) -> &[Recovered<types::Transaction<P>>] {
		self.source().transactions()
	}

	/// Returns the cumulative gas used by the execution of this transaction or
	/// bundle.
	pub fn gas_used(&self) -> u64 {
		self.results.iter().map(|r| r.gas_used()).sum()
	}
}
