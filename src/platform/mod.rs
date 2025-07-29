//! Platform abstraction layer
//!
//! This module fines the platform specific extension points for the undelying
//! node that executes pipelines. By default rblib provides implementations of
//! the standard Ethereum and Optimism platforms, but it can be extended to
//! support other platforms as well.

use {
	crate::{alloy, prelude::*, reth},
	alloy::{
		consensus::{SignableTransaction, Signed},
		network::Network as AlloyNetwork,
		signers::Signature,
	},
	reth::ethereum::primitives::SignedTransaction,
	serde::{Serialize, de::DeserializeOwned},
	std::sync::Arc,
};

mod bundle;
mod ethereum;
mod ext;
mod limits;

pub mod types;
pub use {bundle::*, ethereum::*, ext::*, limits::*};

#[cfg(feature = "optimism")]
mod optimism;

#[cfg(feature = "optimism")]
pub use optimism::*;

/// This type abstracts the platform specific types of the undelying node that
/// is building block payloads.
///
/// The payload builder API is agnostic to the underlying payload types, header
/// types, transaction types, and other platform-specific details. It's primary
/// goal is to enable efficient and flexible payload simulation and
/// construction.
///
/// This trait should be customized for every context this API is embedded in.
pub trait Platform:
	Sized
	+ Default
	+ Clone
	+ Serialize
	+ DeserializeOwned
	+ core::fmt::Debug
	+ Send
	+ Sync
	+ Unpin
	+ 'static
{
	/// Type that configures the essential types of an Ethereum-like node.
	///
	/// Implementations of this trait describe all types that are used by the
	/// consensus engine such as transactions, blocks, headers, etc.
	///
	/// Two well known implementations of this trait are:
	/// - [`EthereumNode`] for Ethereum L1 mainnet,
	/// - [`OpNode`] for Optimism chains.
	type NodeTypes: reth::api::NodeTypes;

	/// A type that provides a complete EVM configuration ready to be used
	/// during the payload execution simulation process. This type is used to
	/// create individual EVM instances that are used to execute transactions.
	type EvmConfig: reth::api::ConfigureEvm<
			Primitives = types::Primitives<Self>,
			NextBlockEnvCtx: Send + Sync + 'static,
		>;

	/// Type that represents transactions that are inside the transaction pool.
	type PooledTransaction: reth::transaction_pool::EthPoolTransaction<
			Consensus = types::Transaction<Self>,
		> + Send
		+ Sync
		+ Unpin
		+ 'static;

	/// Type that configures how bundles are represented and handled by the
	/// platform.
	type Bundle: Bundle<Self>;

	/// Type that can provide limits for the payload building process.
	/// If no limits are set on a pipeline explicitly, a default instance of this
	/// type will be used automatically.
	type DefaultLimits: LimitsFactory<Self> + Default + Send + Sync + 'static;

	/// Instantiate the EVM configuration for the platform with a given chain
	/// specification.
	fn evm_config<P>(chainspec: Arc<types::ChainSpec<P>>) -> Self::EvmConfig
	where
		P: traits::PlatformExecBounds<Self>;

	fn next_block_environment_context<P>(
		chainspec: &types::ChainSpec<P>,
		parent: &types::Header<P>,
		attributes: &types::PayloadBuilderAttributes<P>,
	) -> types::NextBlockEnvContext<P>
	where
		P: traits::PlatformExecBounds<Self>;

	fn build_payload<P, Provider>(
		payload: Checkpoint<P>,
		provider: &Provider,
	) -> Result<types::BuiltPayload<P>, reth::payload::builder::PayloadBuilderError>
	where
		P: traits::PlatformExecBounds<Self>,
		Provider: traits::ProviderBounds<Self>;
}

/// This is an optional extention trait for platforms that want to provide info
/// about their RPC types. Implementing this trait for a platform makes it
/// usable with all `alloy` utilities such as transaction builders, signers, rpc
/// clients, etc.
///
/// In Pipelines some steps require platforms to implement this trait if they
/// produce new transactions as part of their logic and want to remain
/// platform-agnostic, such as [`BuilderEpilogue`].
pub trait PlatformWithRpcTypes: Platform {
	type RpcTypes: AlloyNetwork<
			Header = types::Header<Self>,
			UnsignedTx: SignableTransaction<Signature>,
			TxEnvelope: From<Signed<types::UnsignedTx<Self>, Signature>>
			              + SignedTransaction
			              + From<types::Transaction<Self>>
			              + Into<types::Transaction<Self>>,
		>;
}
