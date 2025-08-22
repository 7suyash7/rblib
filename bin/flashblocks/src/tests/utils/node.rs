use {
	crate::{
		args::{BuilderArgs, FlashblocksArgs},
		builders::pipeline,
		platform::FlashBlocks,
		rpc::TransactionStatusRpc,
	},
	core::{net::SocketAddr, time::Duration},
	rblib::{
		alloy::{
			consensus::BlockHeader,
			eips::BlockNumberOrTag,
			network::BlockResponse,
			providers::Provider,
		},
		pool::*,
		prelude::*,
		reth::optimism::{
			chainspec,
			node::{OpEngineApiBuilder, OpEngineValidatorBuilder, OpNode},
		},
		test_utils::*,
	},
	std::time::{SystemTime, UNIX_EPOCH},
};

impl FlashBlocks {
	pub async fn test_node_with_cli_args(
		cli_args: BuilderArgs,
	) -> eyre::Result<LocalNode<FlashBlocks, OptimismConsensusDriver>> {
		FlashBlocks::create_test_node_with_args(Pipeline::default(), cli_args).await
	}

	pub async fn test_node()
	-> eyre::Result<LocalNode<FlashBlocks, OptimismConsensusDriver>> {
		FlashBlocks::test_node_with_cli_args(BuilderArgs::default()).await
	}

	pub async fn test_node_with_builder_signer()
	-> eyre::Result<LocalNode<FlashBlocks, OptimismConsensusDriver>> {
		FlashBlocks::test_node_with_cli_args(BuilderArgs {
			builder_signer: Some(FundedAccounts::signer(0).into()),
			..Default::default()
		})
		.await
	}

	/// Creates a new flashblocks enabled test node and returns the assigned
	/// socket address of the websocket.
	///
	/// Returns an instance of a local node and the socket address of the
	/// WebSocket.
	///
	/// Flashblocks tests have block times of 2s.
	pub async fn test_node_with_flashblocks_on()
	-> eyre::Result<(LocalNode<FlashBlocks, OptimismConsensusDriver>, SocketAddr)>
	{
		let flashblocks_args = FlashblocksArgs::default_on_for_tests();

		#[allow(clippy::missing_panics_doc)]
		let ws_addr = flashblocks_args.ws_address().expect("default on");

		let mut node = FlashBlocks::test_node_with_cli_args(BuilderArgs {
			flashblocks_args,
			..Default::default()
		})
		.await?;

		node.set_block_time(Duration::from_secs(2));

		Ok((node, ws_addr))
	}
}

pub trait LocalNodeFlashblocksExt {
	async fn while_next_block<F>(
		&self,
		work: F,
	) -> eyre::Result<types::BlockResponse<FlashBlocks>>
	where
		F: Future<Output = eyre::Result<()>> + Send;
}

// async block building
impl LocalNodeFlashblocksExt
	for LocalNode<FlashBlocks, OptimismConsensusDriver>
{
	async fn while_next_block<F>(
		&self,
		work: F,
	) -> eyre::Result<types::BlockResponse<FlashBlocks>>
	where
		F: Future<Output = eyre::Result<()>> + Send,
	{
		let latest_block = self
			.provider()
			.get_block_by_number(BlockNumberOrTag::Latest)
			.await?
			.ok_or_else(|| eyre::eyre!("Failed to get latest block from the node"))?;

		let latest_timestamp =
			Duration::from_secs(latest_block.header().timestamp());

		// calculate the timestamp for the new block
		let current_timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?;
		let elapsed_time = current_timestamp.saturating_sub(latest_timestamp);
		let target_timestamp = latest_timestamp + self.block_time() + elapsed_time;

		// start the payload building process
		let payload_id = self
			.consensus()
			.start_building(self, target_timestamp.as_secs(), &())
			.await?;

		let sleep = tokio::time::sleep(self.block_time());

		tokio::pin!(work, sleep);
		tokio::select! {
			res = &mut work => {
				res?; // propagate work error
				(&mut sleep).await;
			},
			() = &mut sleep => {
				// block time elapsed first; dropping `work` cancels it.
			}
		};

		self
			.consensus()
			.finish_building(self, payload_id, &())
			.await
	}
}

impl TestNodeFactory<FlashBlocks> for FlashBlocks {
	type CliExtArgs = BuilderArgs;
	type ConsensusDriver = OptimismConsensusDriver;

	/// Notes:
	///
	/// - Here we are ignoring the `pipeline` argument because we are not
	///   interested in running arbitrary pipelines for this platform, instead we
	///   construct the pipeline based on the CLI arguments.
	async fn create_test_node_with_args(
		_: Pipeline<FlashBlocks>,
		cli_args: Self::CliExtArgs,
	) -> eyre::Result<LocalNode<FlashBlocks, Self::ConsensusDriver>> {
		let chainspec = chainspec::OP_DEV.as_ref().clone().with_funded_accounts();
		let pool = OrderPool::<FlashBlocks>::default();
		let pipeline = pipeline(&cli_args, &pool)?;

		LocalNode::new(OptimismConsensusDriver, chainspec, move |builder| {
			let opnode = OpNode::new(cli_args.rollup_args.clone());
			let tx_status_rpc = TransactionStatusRpc::new(&pipeline);

			builder
				.with_types::<OpNode>()
				.with_components(
					opnode
						.components()
						.attach_pool(&pool)
						.payload(pipeline.into_service()),
				)
				.with_add_ons(opnode
						.add_ons_builder::<types::RpcTypes<FlashBlocks>>()
						.build::<_, OpEngineValidatorBuilder, OpEngineApiBuilder<OpEngineValidatorBuilder>>())
				.extend_rpc_modules(move |mut rpc_ctx| {
					pool.attach_rpc(&mut rpc_ctx)?;
					tx_status_rpc.attach_rpc(&mut rpc_ctx)?;
					Ok(())
				})
		})
		.await
	}
}
