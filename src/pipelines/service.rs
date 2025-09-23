//! This module implements the machinery that takes a declarative pipeline
//! definition and turns it into a Reth compatible payload builder.

use {
	super::{job::PayloadJob, metrics},
	crate::{
		prelude::*,
		reth::{
			api::PayloadBuilderAttributes,
			builder::{
				BuilderContext,
				NodeConfig,
				components::PayloadServiceBuilder,
			},
			payload::builder::{PayloadBuilderHandle, PayloadBuilderService, *},
			providers::{CanonStateSubscriptions, StateProviderFactory},
		},
	},
	std::sync::Arc,
	tracing::debug,
};

/// This type is the bridge between Reth's payload builder API and the
/// pipelines API. It will take a pipeline instance and turn it into what
/// eventually becomes a Paylod Job Generator. They payload Job Generator
/// will be responsible for creating new [`PayloadJob`] instances
/// whenever a new payload request comes in from the CL Node.
pub(super) struct PipelineServiceBuilder<P: Platform> {
	pipeline: Pipeline<P>,
}

impl<P: Platform> PipelineServiceBuilder<P> {
	pub(super) fn new(pipeline: Pipeline<P>) -> Self {
		Self { pipeline }
	}
}

impl<Plat, Node, Pool> PayloadServiceBuilder<Node, Pool, types::EvmConfig<Plat>>
	for PipelineServiceBuilder<Plat>
where
	Plat: Platform,
	Node: traits::NodeBounds<Plat>,
	Pool: traits::PoolBounds<Plat>,
{
	async fn spawn_payload_builder_service(
		self,
		ctx: &BuilderContext<Node>,
		_: Pool,
		_: types::EvmConfig<Plat>,
	) -> eyre::Result<PayloadBuilderHandle<types::PayloadTypes<Plat>>> {
		let pipeline = self.pipeline;
		debug!("Spawning payload builder service for: {pipeline:#?}");

		let provider = Arc::new(ctx.provider().clone());

		let service = ServiceContext {
			provider: Arc::clone(&provider),
			node_config: ctx.config().clone(),
			metrics: metrics::Payload::with_scope(&format!(
				"{}_payloads",
				pipeline.name()
			)),
		};

		// assign metric names to each step in the pipeline.
		// This will be used to automatically generate runtime observability data
		// during pipeline runs. Also call an optional `setup` function on the step
		// that gives it a chance to perform any initialization it needs before
		// processing any payload jobs.
		let provider = provider as Arc<dyn StateProviderFactory>;

		for step in pipeline.iter_steps() {
			let navi = step.navigator(&pipeline).expect(
				"Invalid step path. This is a bug in the pipeline executor \
				 implementation.",
			);

			let metrics_scope = format!("{}_step_{}", pipeline.name(), navi.path());
			navi.instance().init_metrics(&metrics_scope);

			let init_ctx = InitContext::new(Arc::clone(&provider), metrics_scope);
			navi.instance().setup(init_ctx)?;
		}

		let (service, builder) = PayloadBuilderService::new(
			JobGenerator::new(pipeline, service),
			ctx.provider().canonical_state_stream(),
		);

		ctx
			.task_executor()
			.spawn_critical("payload_builder_service", Box::pin(service));

		Ok(builder)
	}
}

/// There is one service context instance per reth node. This type gives
/// individual jobs access to the node state, transaction pool and other
/// runtime facilities that are managed by reth.
pub(super) struct ServiceContext<Plat, Provider>
where
	Plat: Platform,
	Provider: traits::ProviderBounds<Plat>,
{
	provider: Arc<Provider>,
	node_config: NodeConfig<types::ChainSpec<Plat>>,
	metrics: metrics::Payload,
}

impl<Plat, Provider> ServiceContext<Plat, Provider>
where
	Plat: Platform,
	Provider: traits::ProviderBounds<Plat>,
{
	pub(super) fn provider(&self) -> &Provider {
		&self.provider
	}

	pub(super) const fn node_config(
		&self,
	) -> &NodeConfig<types::ChainSpec<Plat>> {
		&self.node_config
	}

	pub(super) const fn chain_spec(&self) -> &Arc<types::ChainSpec<Plat>> {
		&self.node_config().chain
	}

	pub(super) const fn metrics(&self) -> &metrics::Payload {
		&self.metrics
	}
}

/// This type is stored inside the [`PayloadBuilderService`] type in Reth.
/// There's one instance of this type per node and it is instantiated during the
/// node startup inside `spawn_payload_builder_service`.
///
/// The responsibility of this type is to respond to new payload requests when
/// FCU calls come from the CL Node. Each FCU call will generate a new
/// `PayloadID` on its side and will pass it to the `new_payload_job` method.
pub(super) struct JobGenerator<Plat, Provider>
where
	Plat: Platform,
	Provider: traits::ProviderBounds<Plat>,
{
	pipeline: Arc<Pipeline<Plat>>,
	service: Arc<ServiceContext<Plat, Provider>>,
}

impl<Plat, Provider> JobGenerator<Plat, Provider>
where
	Plat: Platform,
	Provider: traits::ProviderBounds<Plat>,
{
	pub(super) fn new(
		pipeline: Pipeline<Plat>,
		service: ServiceContext<Plat, Provider>,
	) -> Self {
		let pipeline = Arc::new(pipeline);
		let service = Arc::new(service);

		Self { pipeline, service }
	}
}

impl<Plat, Provider> PayloadJobGenerator for JobGenerator<Plat, Provider>
where
	Plat: Platform,
	Provider: traits::ProviderBounds<Plat>,
{
	type Job = PayloadJob<Plat, Provider>;

	fn new_payload_job(
		&self,
		attribs: types::PayloadBuilderAttributes<Plat>,
	) -> Result<Self::Job, PayloadBuilderError> {
		let header = self
			.service
			.provider()
			.sealed_header_by_hash(attribs.parent())?
			.ok_or_else(|| {
				PayloadBuilderError::MissingParentHeader(attribs.parent())
			})?;

		let base_state =
			self.service.provider().state_by_block_hash(header.hash())?;

		// This is the beginning of the state manipulation API usage from within
		// the pipelines API.
		let block_ctx = BlockContext::new(
			header,
			attribs,
			base_state,
			self.service.chain_spec().clone(),
		)
		.map_err(PayloadBuilderError::other)?;

		Ok(PayloadJob::new(&self.pipeline, block_ctx, &self.service))
	}
}
