use {
	super::exec::IntoExecutable,
	crate::*,
	alloy::{
		consensus::crypto::RecoveryError,
		primitives::{Address, B256, StorageValue},
	},
	core::fmt::{Debug, Display},
	reth::{
		core::primitives::SignedTransaction,
		errors::ProviderError,
		primitives::Recovered,
		revm::{
			DatabaseRef,
			primitives::StorageKey,
			state::{AccountInfo, Bytecode},
		},
	},
	reth_origin::revm::db::BundleState,
	std::sync::Arc,
	thiserror::Error,
};

#[derive(Debug, Error)]
pub enum Error<P: Platform> {
	#[error("Failed to recover signature for transaction")]
	SignatureRecovery(#[from] RecoveryError),

	#[error("Evm execution error: {0}")]
	Evm(types::EvmError<P, ProviderError>),
}

/// Checkpoints represent an atomic incremental change in the payload building
/// process.
///
/// Notes:
///  - There is no public API to create a checkpoint directly. Checkpoints are
///    created by the [`BlockContext`] when it starts a new payload building
///    process or by mutatations applied to an already existing checkpoint.
///
///  - Checkpoints contain all the information needed to assemble a full block
///    payload, they however cannot be used directly to assemble a block. The
///    block assembly process is very node-specific and is part of the pipelines
///    api, which has more info and access to the underlying node facilities.
///
///  - Checkpoints are immutable, meaning that once a checkpoint is created, it
///    cannot be changed. Instead, new checkpoints can be created on top of the
///    existing ones, forming a chain of checkpoints.
///
///  - Checkpoints may represent forks in the payload building process. Two
///    checkpoints can share a common ancestor, without having linear history
///    between them. Each of the diverging checkpoints can be used to build
///    alternative versions of the payload.
///
///  - Checkpoints are cheap to clone, discard and move around. They are
///    expensive to create, as they require executing a transaction by the EVM
///    and storing the resulting state changes.
///
///  - Checkpoints are thread-safe, Send + Sync + 'static.
///
///  - Checkpoints are always in a state that can be used to build a valid block
///    payload. You can't create checkpoints with invalid transactions (such as
///    invalid nonces, invalid signatures, etc.) that would invalidate the block
///    payload validity according to consensus rules.
///
///  - Checkpoints are state providers, meaning that any checkpoint can be used
///    as a database reference in an input to an EVM instance when simulating
///    transactions. The state of the checkpoint is the cumulative state of all
///    state mutations applied since the beginning of the block payload,
///    including the base state of the parent block of the block for which the
///    payload is being built.
pub struct Checkpoint<P: Platform> {
	inner: Arc<CheckpointInner<P>>,
}

/// Public read API
impl<P: Platform> Checkpoint<P> {
	/// Returns the number of checkpoints preceeding this checkpoint since the
	/// beginning of the block payload we're building.
	///
	/// Depth zero is when [`BlockContext::start`] is called, and the first
	/// checkpoint is created and has no previous checkpoints.
	pub fn depth(&self) -> usize {
		self.inner.depth
	}

	/// The returns the payload version before the current checkpoint.
	/// Working with the previous checkpoint is equivalent to discarding the
	/// state mutations made in the current checkpoint.
	///
	/// There may be multiple parallel forks of the payload under construction,
	/// rooted at the same checkpoint.
	pub fn prev(&self) -> Option<Checkpoint<P>> {
		self.inner.prev.as_ref().map(|prev| Checkpoint {
			inner: Arc::clone(prev),
		})
	}

	/// Returns the block context that is the root of theis checkpoint.
	pub fn block(&self) -> &BlockContext<P> {
		&self.inner.block
	}

	/// The transactions that created this checkpoint. This could be either an
	/// empty iterator if this checkpoint is a barrier or other non-transaction
	/// checkpoint, it can be one transaction if this checkpoint was created by
	/// applying a single transaction, or it can be multiple if this checkpoint
	/// represents a bundle.
	pub fn transactions(&self) -> &[Recovered<types::Transaction<P>>] {
		match &self.inner.mutation {
			Mutation::Barrier => &[],
			Mutation::Executable(result) => result.transactions(),
		}
	}

	/// The execution result(s) of the transaction(s) that created this
	/// checkpoint.
	pub fn result(&self) -> Option<&ExecutionResult<P>> {
		match &self.inner.mutation {
			Mutation::Barrier => None,
			Mutation::Executable(result) => Some(result),
		}
	}

	/// The state changes that occured as a result of executing the
	/// transaction(s) that created this checkpoint.
	pub fn state(&self) -> Option<&BundleState> {
		match self.inner.mutation {
			Mutation::Barrier => None,
			Mutation::Executable(ref result) => Some(result.state()),
		}
	}

	/// Returns true if this checkpoint is a barrier checkpoint.
	pub fn is_barrier(&self) -> bool {
		matches!(self.inner.mutation, Mutation::Barrier)
	}

	/// If this checkpoint is a single transaction, returns a reference to the
	/// transaction that created this checkpoint. otherwise returns `None`.
	pub fn as_transaction(&self) -> Option<&Recovered<types::Transaction<P>>> {
		if let Mutation::Executable(result) = &self.inner.mutation {
			if let Executable::Transaction(tx) = result.source() {
				return Some(tx);
			}
		}

		None
	}

	/// If this checkpoint is a bundle, returns a reference to the bundle that
	/// created this checkpoint. otherwise returns `None`.
	pub fn as_bundle(&self) -> Option<&types::Bundle<P>> {
		if let Mutation::Executable(result) = &self.inner.mutation {
			if let Executable::Bundle(bundle) = result.source() {
				return Some(bundle);
			}
		}

		None
	}
}

/// Public builder API
impl<P: Platform> Checkpoint<P> {
	/// Creates a new checkpoint on top of the current checkpoint by applying a
	/// transaction or a bundle of transactions. The execution will use the
	/// cumulative state of all checkpoints in the current checkpoint history as
	/// its state.
	pub fn apply<S>(
		&self,
		executable: impl IntoExecutable<P, S>,
	) -> Result<Self, ExecutionError<P>> {
		Ok(Self {
			inner: Arc::new(CheckpointInner {
				block: self.inner.block.clone(),
				prev: Some(Arc::clone(&self.inner)),
				depth: self.inner.depth + 1,
				mutation: Mutation::Executable(
					executable
						.try_into_executable()?
						.execute(self.block(), self)?,
				),
			}),
		})
	}

	/// Creates a new checkpoint on top of the current checkpoint that introduces
	/// a barrier. This new checkpoint will be now considered the new beginning of
	/// mutable history.
	#[must_use]
	pub fn barrier(&self) -> Self {
		Self {
			inner: Arc::new(CheckpointInner {
				block: self.inner.block.clone(),
				prev: Some(Arc::clone(&self.inner)),
				depth: self.inner.depth + 1,
				mutation: Mutation::Barrier,
			}),
		}
	}
}

/// Internal API
impl<P: Platform> Checkpoint<P> {
	/// Start a new checkpoint for an empty payload rooted at the
	/// state of the parent block of the block for which the payload is
	/// being built.
	pub(super) fn new_at_block(block: BlockContext<P>) -> Self {
		Self {
			inner: Arc::new(CheckpointInner {
				block,
				prev: None,
				depth: 0,
				mutation: Mutation::Barrier,
			}),
		}
	}
}

/// Describes the type of state mutation that was applied to the
/// previous checkpoint to create this checkpoint.
enum Mutation<P: Platform> {
	/// A checkpoint that indicates that any prior checkpoints are immutable and
	/// should not be discarded or reordered. An example of this would be placing
	/// a barrier after applying sequencer transactions, to ensure that they do
	/// not get reordered by pipelines. Another example would be placing a barrier
	/// after every commited flashblock, to ensure that any steps in the pipeline
	/// do not modify the commited state of the payload in process.
	///
	/// If there are multiple barriers in the history, the last one is considered
	/// as the beginning of the mutable history.
	///
	/// The very first checkpoint in the history is always a barrier, as it
	/// represents the baseline checkpoint that has no transactions in its
	/// history.
	Barrier,

	/// A checkpoint that was created by applying one executable item on top of
	/// the previous checkpoint. The executable item can be a single transaction
	/// or a bundle of transactions.
	Executable(ExecutionResult<P>),
}

struct CheckpointInner<P: Platform> {
	/// The block context for which this checkpoint was created.
	block: BlockContext<P>,

	/// The previous checkpoint in this chain of checkpoints, if any.
	prev: Option<Arc<Self>>,

	/// The number of checkpoints in the chain starting from the begining of the
	/// block context.
	///
	/// Depth zero is when [`BlockContext::start`] is called, and the first
	depth: usize,

	/// The mutation
	mutation: Mutation<P>,
}

/// Converts a checkpoint into a vector of transactions that were applied to
/// it.
impl<P: Platform> From<Checkpoint<P>> for Vec<types::Transaction<P>> {
	fn from(checkpoint: Checkpoint<P>) -> Self {
		checkpoint
			.history()
			.transactions()
			.map(Recovered::clone_inner)
			.collect()
	}
}

/// Any checkpoint can be used as a database reference for an EVM instance.
/// The state at a checkpoint is the cumulative aggregate of all state mutations
/// that occured in the current checkpoint and all its ancestors on top of the
/// base state of the parent block of the block for which the payload is being
/// built.
impl<P: Platform> DatabaseRef for Checkpoint<P> {
	/// The database error type.
	type Error = ProviderError;

	/// Gets basic account information.
	fn basic_ref(
		&self,
		address: Address,
	) -> Result<Option<AccountInfo>, Self::Error> {
		// we want to probe the history of checkpoints in reverse order,
		// starting from the most recent one, to find the first checkpoint
		// that has touched the given address.

		let mut current = Some(&self.inner);
		while let Some(checkpoint) = current {
			if let Mutation::Executable(result) = &checkpoint.mutation {
				if let Some(account) = result
					.state()
					.account(&address)
					.and_then(|account| account.info.as_ref())
				{
					return Ok(Some(account.clone()));
				}
			}

			current = checkpoint.prev.as_ref();
		}

		// none of the checkpoints priori to this have touched this address,
		// now we need to check if the account exists in the base state of the
		// block context.
		if let Some(acc) = self.block().base_state().basic_account(&address)? {
			return Ok(Some(acc.into()));
		}

		// account does not exist
		Ok(None)
	}

	/// Gets account code by its hash.
	fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
		// we want to probe the history of checkpoints in reverse order,
		// starting from the most recent one, to find the first checkpoint
		// that has created the code with the given hash.

		let mut current = Some(&self.inner);
		while let Some(checkpoint) = current {
			if let Mutation::Executable(result) = &checkpoint.mutation {
				if let Some(code) = result.state().bytecode(&code_hash) {
					return Ok(code);
				}
			}

			current = checkpoint.prev.as_ref();
		}

		Ok(
			self
				.block()
				.base_state()
				.bytecode_by_hash(&code_hash)?
				.unwrap_or_default()
				.0,
		)
	}

	/// Gets storage value of address at index.
	fn storage_ref(
		&self,
		address: Address,
		index: StorageKey,
	) -> Result<StorageValue, Self::Error> {
		// traverse checkpoints history looking for the first checkpoint that
		// has touched the given address.

		let mut current = Some(&self.inner);
		while let Some(checkpoint) = current {
			if let Mutation::Executable(result) = &checkpoint.mutation {
				if let Some(slot) = result
					.state()
					.account(&address)
					.and_then(|account| account.storage.get(&index))
				{
					return Ok(slot.present_value);
				}
			}

			current = checkpoint.prev.as_ref();
		}

		// none of the checkpoints prior to this have touched this address,
		// now we need to check if the account exists in the base state of the
		// block context.
		Ok(
			self
				.block()
				.base_state()
				.storage(address, index.into())?
				.unwrap_or_default(),
		)
	}

	/// Gets block hash by block number.
	fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
		Ok(
			self
				.block()
				.base_state()
				.block_hash(number)?
				.unwrap_or_default(),
		)
	}
}

impl<P: Platform> Clone for Checkpoint<P> {
	fn clone(&self) -> Self {
		Self {
			inner: Arc::clone(&self.inner),
		}
	}
}

impl<P: Platform> PartialEq for Checkpoint<P> {
	fn eq(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.inner, &other.inner)
	}
}
impl<P: Platform> Eq for Checkpoint<P> {}

impl<P: Platform> Debug for Checkpoint<P> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.debug_struct("Checkpoint")
			.field("depth", &self.depth() as &dyn Debug)
			.field(
				"block",
				&format!("{} + 1", self.block().parent().hash()) as &dyn Debug,
			)
			.field(
				"txs",
				&self
					.transactions()
					.iter()
					.map(|tx| tx.tx_hash())
					.collect::<Vec<_>>() as &dyn Debug,
			)
			.field("result", &self.result() as &dyn Debug)
			.finish()
	}
}

impl<P: Platform> Display for Checkpoint<P> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		let Mutation::Executable(exec_result) = &self.inner.mutation else {
			if self.depth() == 0 {
				// this is the initial checkpoint
				return write!(f, "[{}] initial", self.depth());
			}

			// this is a barrier checkpoint, which has no transactions
			// applied to it.
			return write!(f, "[{}] barrier", self.depth());
		};

		match exec_result.source() {
			Executable::Transaction(tx) => write!(
				f,
				"[{}] tx {} ({}, {} gas)",
				self.depth(),
				tx.tx_hash(),
				match exec_result.results()[0] {
					types::TransactionExecutionResult::<P>::Success { .. } => "success",
					types::TransactionExecutionResult::<P>::Revert { .. } => "revert",
					types::TransactionExecutionResult::<P>::Halt { .. } => "halt",
				},
				self.gas_used(),
			),
			Executable::Bundle(bundle) => write!(
				f,
				"[{}] (bundle {} txs, {} gas)",
				self.depth(),
				bundle.transactions().len(),
				self.gas_used(),
			),
		}
	}
}
