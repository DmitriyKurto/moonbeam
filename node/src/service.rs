// Copyright 2019-2020 PureStake Inc.
// This file is part of Moonbeam.

// Moonbeam is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Moonbeam is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Moonbeam.  If not, see <http://www.gnu.org/licenses/>.
use crate::{cli::Sealing, inherents::build_inherent_data_providers};
use async_io::Timer;
use cumulus_client_consensus_relay_chain::{
	build_relay_chain_consensus, BuildRelayChainConsensusParams,
};
use cumulus_client_network::build_block_announce_validator;
use cumulus_client_service::{
	prepare_node_config, start_collator, start_full_node, StartCollatorParams, StartFullNodeParams,
};
use fc_consensus::FrontierBlockImport;
use fc_mapping_sync::MappingSyncWorker;
use fc_rpc::EthTask;
use fc_rpc_core::types::{FilterPool, PendingTransactions};
use futures::{Stream, StreamExt};
use moonbeam_runtime::{opaque::Block, RuntimeApi};
use polkadot_primitives::v0::CollatorPair;
use sc_consensus_manual_seal::{run_manual_seal, EngineCommand, ManualSealParams};
use sc_executor::native_executor_instance;
pub use sc_executor::NativeExecutor;
use sc_service::{
	error::Error as ServiceError, Configuration, PartialComponents, Role, TFullBackend,
	TFullClient, TaskManager, BasePath,
};
use sc_cli::SubstrateCli;
use sp_core::{Pair, H160, H256};
use sp_runtime::traits::BlakeTwo256;
use sp_trie::PrefixedMemoryDB;
use std::{
	collections::{BTreeMap, HashMap},
	sync::{Arc, Mutex},
	time::Duration,
};

// Our native executor instance.
native_executor_instance!(
	pub Executor,
	moonbeam_runtime::api::dispatch,
	moonbeam_runtime::native_version,
);
use sc_telemetry::{Telemetry, TelemetryWorker, TelemetryWorkerHandle};

type FullClient = TFullClient<Block, RuntimeApi, Executor>;
type FullBackend = TFullBackend<Block>;

pub fn open_frontier_backend(config: &Configuration) -> Result<Arc<fc_db::Backend<Block>>, String> {
	let config_dir = config.base_path.as_ref()
		.map(|base_path| base_path.config_dir(config.chain_spec.id()))
		.unwrap_or_else(|| {
			BasePath::from_project("", "", &crate::cli::Cli::executable_name())
				.config_dir(config.chain_spec.id())
		});
	let database_dir = config_dir.join("frontier").join("db");

	Ok(Arc::new(fc_db::Backend::<Block>::new(&fc_db::DatabaseSettings {
		source: fc_db::DatabaseSettingsSrc::RocksDb {
			path: database_dir,
			cache_size: 0,
		}
	})?))
}

/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
#[allow(clippy::type_complexity)]
pub fn new_partial(
	config: &Configuration,
	author: Option<H160>,
	mock_inherents: bool,
) -> Result<
	PartialComponents<
		FullClient,
		FullBackend,
		(),
		sp_consensus::import_queue::BasicQueue<Block, PrefixedMemoryDB<BlakeTwo256>>,
		sc_transaction_pool::FullPool<Block, FullClient>,
		(
			FrontierBlockImport<Block, Arc<FullClient>, FullClient>,
			PendingTransactions,
			Option<FilterPool>,
			Option<Telemetry>,
			Option<TelemetryWorkerHandle>,
			Arc<fc_db::Backend<Block>>,
		),
	>,
	ServiceError,
> {
	let inherent_data_providers = build_inherent_data_providers(author, mock_inherents)?;

	let telemetry = config.telemetry_endpoints.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, Executor>(
			&config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
		)?;

	let client = Arc::new(client);

	let telemetry_worker_handle = telemetry
		.as_ref()
		.map(|(worker, _)| worker.handle());

	let telemetry = telemetry
		.map(|(worker, telemetry)| {
			task_manager.spawn_handle().spawn("telemetry", worker.run());
			telemetry
		});

	let registry = config.prometheus_registry();

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_handle(),
		client.clone(),
	);

	let pending_transactions: PendingTransactions = Some(Arc::new(Mutex::new(HashMap::new())));

	let filter_pool: Option<FilterPool> = Some(Arc::new(Mutex::new(BTreeMap::new())));

	let frontier_backend = open_frontier_backend(config)?;

	let frontier_block_import = FrontierBlockImport::new(
		client.clone(),
		client.clone(),
		frontier_backend.clone(),
	);

	// We use the cumulus import queue here regardless of whether we're running a real parachain or
	// the dev service. Typically manual seal would use the manual seal import queue. In reality,
	// both are pretty minimal. The cumulus one checks the inherents, so we use it.
	// https://github.com/paritytech/substrate/issues/8164
	let import_queue = cumulus_client_consensus_relay_chain::import_queue(
		client.clone(),
		frontier_block_import.clone(),
		inherent_data_providers.clone(),
		&task_manager.spawn_essential_handle(),
		registry,
	)?;

	Ok(PartialComponents {
		backend,
		client,
		import_queue,
		keystore_container,
		task_manager,
		transaction_pool,
		inherent_data_providers,
		select_chain: (),
		other: (
			frontier_block_import,
			pending_transactions,
			filter_pool,
			telemetry,
			telemetry_worker_handle,
			frontier_backend,
		),
	})
}

/// Start a node with the given parachain `Configuration` and relay chain `Configuration`.
///
/// This is the actual implementation that is abstract over the executor and the runtime api.
async fn start_node_impl<RB>(
	parachain_config: Configuration,
	collator_key: CollatorPair,
	author_id: Option<H160>,
	polkadot_config: Configuration,
	id: polkadot_primitives::v0::Id,
	collator: bool,
	_rpc_ext_builder: RB,
) -> sc_service::error::Result<(TaskManager, Arc<FullClient>)>
where
	RB: Fn(
		Arc<TFullClient<Block, RuntimeApi, Executor>>,
	) -> jsonrpc_core::IoHandler<sc_rpc::Metadata>
		+ Send
		+ 'static,
{
	if matches!(parachain_config.role, Role::Light) {
		return Err("Light client not supported!".into());
	}

	let parachain_config = prepare_node_config(parachain_config);

	let params = new_partial(&parachain_config, author_id, false)?;
	let (
		block_import,
		pending_transactions,
		filter_pool,
		mut telemetry,
		telemetry_worker_handle,
		frontier_backend,
	) = params.other;

	let polkadot_full_node =
		cumulus_client_service::build_polkadot_full_node(
			polkadot_config,
			collator_key.public(),
			telemetry_worker_handle,
		)
		.map_err(|e| match e {
			polkadot_service::Error::Sub(x) => x,
			s => format!("{}", s).into(),
		})?;

	let client = params.client.clone();
	let backend = params.backend.clone();
	let block_announce_validator = build_block_announce_validator(
		polkadot_full_node.client.clone(),
		id,
		Box::new(polkadot_full_node.network.clone()),
		polkadot_full_node.backend.clone(),
	);

	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let transaction_pool = params.transaction_pool.clone();
	let mut task_manager = params.task_manager;
	let import_queue = params.import_queue;
	let (network, network_status_sinks, system_rpc_tx, start_network) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &parachain_config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			on_demand: None,
			block_announce_validator_builder: Some(Box::new(|_| block_announce_validator)),
		})?;

	let subscription_task_executor =
		sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();
		let network = network.clone();
		let pending = pending_transactions.clone();
		let filter_pool = filter_pool.clone();
		Box::new(move |deny_unsafe, _| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				graph: pool.pool().clone(),
				deny_unsafe,
				is_authority: collator,
				network: network.clone(),
				pending_transactions: pending.clone(),
				filter_pool: filter_pool.clone(),
				command_sink: None,
				backend: frontier_backend.clone(),
			};

			crate::rpc::create_full(deps, subscription_task_executor.clone())
		})
	};

	task_manager.spawn_essential_handle().spawn(
		"frontier-mapping-sync-worker",
		MappingSyncWorker::new(
			client.import_notification_stream(),
			Duration::new(6, 0),
			client.clone(),
			backend.clone(),
			frontier_backend.clone(),
		).for_each(|()| futures::future::ready(()))
	);

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		on_demand: None,
		remote_blockchain: None,
		rpc_extensions_builder,
		client: client.clone(),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		config: parachain_config,
		keystore: params.keystore_container.sync_keystore(),
		backend: backend.clone(),
		network: network.clone(),
		network_status_sinks,
		system_rpc_tx,
		telemetry: telemetry.as_mut(),
	})?;

	// Spawn Frontier EthFilterApi maintenance task.
	if let Some(filter_pool) = filter_pool {
		// Each filter is allowed to stay in the pool for 100 blocks.
		const FILTER_RETAIN_THRESHOLD: u64 = 100;
		task_manager.spawn_essential_handle().spawn(
			"frontier-filter-pool",
			EthTask::filter_pool_task(
				Arc::clone(&client),
				filter_pool,
				FILTER_RETAIN_THRESHOLD,
			)
		);
	}

	// Spawn Frontier pending transactions maintenance task (as essential, otherwise we leak).
	if let Some(pending_transactions) = pending_transactions {
		const TRANSACTION_RETAIN_THRESHOLD: u64 = 5;
		task_manager.spawn_essential_handle().spawn(
			"frontier-pending-transactions",
			EthTask::pending_transaction_task(
				Arc::clone(&client),
				pending_transactions,
				TRANSACTION_RETAIN_THRESHOLD,
			)
		);
	}

	let announce_block = {
		let network = network.clone();
		Arc::new(move |hash, data| network.announce_block(hash, data))
	};

	if collator {
		let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool,
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|x| x.handle()),
		);
		let spawner = task_manager.spawn_handle();

		let parachain_consensus = build_relay_chain_consensus(BuildRelayChainConsensusParams {
			para_id: id,
			proposer_factory,
			inherent_data_providers: params.inherent_data_providers,
			block_import,
			relay_chain_client: polkadot_full_node.client.clone(),
			relay_chain_backend: polkadot_full_node.backend.clone(),
		});

		let params = StartCollatorParams {
			para_id: id,
			block_status: client.clone(),
			announce_block,
			client: client.clone(),
			task_manager: &mut task_manager,
			collator_key,
			spawner,
			backend,
			relay_chain_full_node: polkadot_full_node,
			parachain_consensus,
		};

		start_collator(params).await?;
	} else {
		let params = StartFullNodeParams {
			client: client.clone(),
			announce_block,
			task_manager: &mut task_manager,
			para_id: id,
			polkadot_full_node,
		};

		start_full_node(params)?;
	}

	start_network.start_network();

	Ok((task_manager, client))
}

/// Start a normal parachain node.
pub async fn start_node(
	parachain_config: Configuration,
	collator_key: CollatorPair,
	author_id: Option<H160>,
	polkadot_config: Configuration,
	id: polkadot_primitives::v0::Id,
	collator: bool,
) -> sc_service::error::Result<(TaskManager, Arc<FullClient>)> {
	start_node_impl(
		parachain_config,
		collator_key,
		author_id,
		polkadot_config,
		id,
		collator,
		|_| Default::default(),
	)
	.await
}

/// Builds a new development service. This service uses manual seal, and mocks
/// the parachain inherent.
pub fn new_dev(
	config: Configuration,
	sealing: Sealing,
	author_id: Option<H160>,
	collator: bool,
) -> Result<TaskManager, ServiceError> {
	let sc_service::PartialComponents {
		client,
		backend,
		mut task_manager,
		import_queue,
		keystore_container,
		select_chain: _,
		transaction_pool,
		inherent_data_providers,
		other: (
			block_import,
			pending_transactions,
			filter_pool,
			telemetry,
			_telemetry_worker_handle,
			frontier_backend,
		),
	} = new_partial(&config, author_id, true)?;

	let (network, network_status_sinks, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			on_demand: None,
			block_announce_validator_builder: None,
		})?;

	if config.offchain_worker.enabled {
		sc_service::build_offchain_workers(
			&config,
			task_manager.spawn_handle(),
			client.clone(),
			network.clone(),
		);
	}

	let prometheus_registry = config.prometheus_registry().cloned();
	let subscription_task_executor =
		sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());
	let mut command_sink = None;

	if collator {
		let env = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool.clone(),
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|x| x.handle()),
		);

		let commands_stream: Box<dyn Stream<Item = EngineCommand<H256>> + Send + Sync + Unpin> =
			match sealing {
				Sealing::Instant => {
					Box::new(
						// This bit cribbed from the implementation of instant seal.
						transaction_pool
							.pool()
							.validated_pool()
							.import_notification_stream()
							.map(|_| EngineCommand::SealNewBlock {
								create_empty: false,
								finalize: false,
								parent_hash: None,
								sender: None,
							}),
					)
				}
				Sealing::Manual => {
					let (sink, stream) = futures::channel::mpsc::channel(1000);
					// Keep a reference to the other end of the channel. It goes to the RPC.
					command_sink = Some(sink);
					Box::new(stream)
				}
				Sealing::Interval(millis) => Box::new(StreamExt::map(
					Timer::interval(Duration::from_millis(millis)),
					|_| EngineCommand::SealNewBlock {
						create_empty: true,
						finalize: false,
						parent_hash: None,
						sender: None,
					},
				)),
			};

		// Typically the longest chain rule is constructed in `new_partial`, but the full service,
		// does not use one. So instead we construct our longest chain rule here.
		let select_chain = sc_consensus::LongestChain::new(backend.clone());

		task_manager.spawn_essential_handle().spawn_blocking(
			"authorship_task",
			run_manual_seal(ManualSealParams {
				block_import,
				env,
				client: client.clone(),
				pool: transaction_pool.pool().clone(),
				commands_stream,
				select_chain,
				consensus_data_provider: None,
				inherent_data_providers,
			}),
		);
	}

	let rpc_extensions_builder = {
		let client = client.clone();
		let pool = transaction_pool.clone();
		let network = network.clone();
		let pending = pending_transactions.clone();
		let filter_pool = filter_pool.clone();
		Box::new(move |deny_unsafe, _| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: pool.clone(),
				graph: pool.pool().clone(),
				deny_unsafe,
				is_authority: collator,
				network: network.clone(),
				pending_transactions: pending.clone(),
				filter_pool: filter_pool.clone(),
				command_sink: command_sink.clone(),
				backend: frontier_backend.clone(),
			};
			crate::rpc::create_full(deps, subscription_task_executor.clone())
		})
	};

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		network,
		client: client.clone(),
		keystore: keystore_container.sync_keystore(),
		task_manager: &mut task_manager,
		transaction_pool: transaction_pool.clone(),
		rpc_extensions_builder,
		on_demand: None,
		remote_blockchain: None,
		backend,
		network_status_sinks,
		system_rpc_tx,
		config,
		telemetry: None,
	})?;

	// Spawn Frontier EthFilterApi maintenance task.
	if let Some(filter_pool) = filter_pool {
		// Each filter is allowed to stay in the pool for 100 blocks.
		const FILTER_RETAIN_THRESHOLD: u64 = 100;
		task_manager.spawn_essential_handle().spawn(
			"frontier-filter-pool",
			EthTask::filter_pool_task(
				Arc::clone(&client),
				filter_pool,
				FILTER_RETAIN_THRESHOLD,
			)
		);
	}

	// Spawn Frontier pending transactions maintenance task (as essential, otherwise we leak).
	if let Some(pending_transactions) = pending_transactions {
		const TRANSACTION_RETAIN_THRESHOLD: u64 = 5;
		task_manager.spawn_essential_handle().spawn(
			"frontier-pending-transactions",
			EthTask::pending_transaction_task(
				Arc::clone(&client),
				pending_transactions,
				TRANSACTION_RETAIN_THRESHOLD,
			)
		);
	}

	log::info!("Development Service Ready");

	network_starter.start_network();
	Ok(task_manager)
}
