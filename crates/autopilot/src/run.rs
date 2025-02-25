use {
    crate::{
        arguments::Arguments,
        database::{
            ethflow_events::event_retriever::EthFlowRefundRetriever,
            onchain_order_events::{
                ethflow_events::{determine_ethflow_indexing_start, EthFlowOnchainOrderParser},
                event_retriever::CoWSwapOnchainOrdersContract,
                OnchainOrderParser,
            },
            Postgres,
        },
        driver_api::Driver,
        event_updater::{EventUpdater, GPv2SettlementContract},
        infra::{self, blockchain},
        protocol,
        run_loop::RunLoop,
        shadow,
        solvable_orders::SolvableOrdersCache,
    },
    clap::Parser,
    contracts::{BalancerV2Vault, IUniswapV3Factory, WETH9},
    ethcontract::{errors::DeployError, BlockNumber},
    ethrpc::current_block::block_number_to_block_number_hash,
    futures::StreamExt,
    model::DomainSeparator,
    shared::{
        account_balances,
        bad_token::{
            cache::CachingDetector,
            instrumented::InstrumentedBadTokenDetectorExt,
            list_based::{ListBasedDetector, UnknownTokenStrategy},
            token_owner_finder,
            trace_call::TraceCallDetector,
        },
        baseline_solver::BaseTokens,
        fee_subsidy::{config::FeeSubsidyConfiguration, FeeSubsidizing},
        gas_price::InstrumentedGasEstimator,
        http_client::HttpClientFactory,
        maintenance::{Maintaining, ServiceMaintenance},
        metrics::LivenessChecking,
        oneinch_api::OneInchClientImpl,
        order_quoting::{self, OrderQuoter},
        price_estimation::factory::{self, PriceEstimatorFactory, PriceEstimatorSource},
        recent_block_cache::CacheConfig,
        signature_validator,
        sources::{
            balancer_v2::{
                pool_fetching::BalancerContracts,
                BalancerFactoryKind,
                BalancerPoolFetcher,
            },
            uniswap_v2::{pool_cache::PoolCache, UniV2BaselineSourceParameters},
            uniswap_v3::pool_fetching::UniswapV3PoolFetcher,
            BaselineSource,
            PoolAggregator,
        },
        token_info::{CachedTokenInfoFetcher, TokenInfoFetcher},
        token_list::{AutoUpdatingTokenList, TokenListConfiguration},
        zeroex_api::DefaultZeroExApi,
    },
    std::{collections::HashSet, sync::Arc, time::Duration},
    tracing::Instrument,
    url::Url,
};

struct Liveness {
    solvable_orders_cache: Arc<SolvableOrdersCache>,
    max_auction_age: Duration,
}

#[async_trait::async_trait]
impl LivenessChecking for Liveness {
    async fn is_alive(&self) -> bool {
        let age = self.solvable_orders_cache.last_update_time().elapsed();
        age <= self.max_auction_age
    }
}

async fn ethrpc(url: &Url) -> blockchain::Rpc {
    blockchain::Rpc::new(url)
        .await
        .expect("connect ethereum RPC")
}

async fn ethereum(ethrpc: blockchain::Rpc) -> blockchain::Ethereum {
    blockchain::Ethereum::new(ethrpc).await
}

pub async fn start(args: impl Iterator<Item = String>) {
    let args = Arguments::parse_from(args);
    observe::tracing::initialize(
        args.shared.logging.log_filter.as_str(),
        args.shared.logging.log_stderr_threshold,
    );
    observe::panic_hook::install();
    tracing::info!("running autopilot with validated arguments:\n{}", args);
    observe::metrics::setup_registry(Some("gp_v2_autopilot".into()), None);

    if args.drivers.is_empty() {
        panic!("colocation is enabled but no drivers are configured");
    }

    if args.shadow.is_some() {
        shadow_mode(args).await;
    } else {
        run(args).await;
    }
}

/// Assumes tracing and metrics registry have already been set up.
pub async fn run(args: Arguments) {
    assert!(args.shadow.is_none(), "cannot run in shadow mode");

    let db = Postgres::new(args.db_url.as_str(), args.order_events_insert_batch_size)
        .await
        .unwrap();
    crate::database::run_database_metrics_work(db.clone());

    let http_factory = HttpClientFactory::new(&args.http_client);
    let web3 = shared::ethrpc::web3(
        &args.shared.ethrpc,
        &http_factory,
        &args.shared.node_url,
        "base",
    );
    let simulation_web3 = args.shared.simulation_node_url.as_ref().map(|node_url| {
        shared::ethrpc::web3(&args.shared.ethrpc, &http_factory, node_url, "simulation")
    });

    let chain_id = web3
        .eth()
        .chain_id()
        .await
        .expect("Could not get chainId")
        .as_u64();
    if let Some(expected_chain_id) = args.shared.chain_id {
        assert_eq!(
            chain_id, expected_chain_id,
            "connected to node with incorrect chain ID",
        );
    }

    let current_block_stream = args
        .shared
        .current_block
        .stream(web3.clone())
        .await
        .unwrap();

    let settlement_contract = match args.shared.settlement_contract_address {
        Some(address) => contracts::GPv2Settlement::with_deployment_info(&web3, address, None),
        None => contracts::GPv2Settlement::deployed(&web3)
            .await
            .expect("load settlement contract"),
    };
    let vault_relayer = settlement_contract
        .vault_relayer()
        .call()
        .await
        .expect("Couldn't get vault relayer address");
    let native_token = match args.shared.native_token_address {
        Some(address) => contracts::WETH9::with_deployment_info(&web3, address, None),
        None => WETH9::deployed(&web3)
            .await
            .expect("load native token contract"),
    };
    let vault = match args.shared.balancer_v2_vault_address {
        Some(address) => Some(contracts::BalancerV2Vault::with_deployment_info(
            &web3, address, None,
        )),
        None => match BalancerV2Vault::deployed(&web3).await {
            Ok(contract) => Some(contract),
            Err(DeployError::NotFound(_)) => {
                tracing::warn!("balancer contracts are not deployed on this network");
                None
            }
            Err(err) => panic!("failed to get balancer vault contract: {err}"),
        },
    };
    let uniswapv3_factory = match IUniswapV3Factory::deployed(&web3).await {
        Err(DeployError::NotFound(_)) => None,
        other => Some(other.unwrap()),
    };

    let network = web3
        .net()
        .version()
        .await
        .expect("Failed to retrieve network version ID");
    let network_name = shared::network::network_name(&network, chain_id);

    let signature_validator = signature_validator::validator(
        &web3,
        signature_validator::Contracts {
            chain_id,
            settlement: settlement_contract.address(),
            vault_relayer,
        },
    );

    let balance_fetcher = account_balances::cached(
        &web3,
        account_balances::Contracts {
            chain_id,
            settlement: settlement_contract.address(),
            vault_relayer,
            vault: vault.as_ref().map(|contract| contract.address()),
        },
        current_block_stream.clone(),
    );

    let gas_price_estimator = Arc::new(
        shared::gas_price_estimation::create_priority_estimator(
            &http_factory,
            &web3,
            args.shared.gas_estimators.as_slice(),
            args.shared.blocknative_api_key.clone(),
        )
        .await
        .expect("failed to create gas price estimator"),
    );

    let baseline_sources = args.shared.baseline_sources.clone().unwrap_or_else(|| {
        shared::sources::defaults_for_chain(chain_id)
            .expect("failed to get default baseline sources")
    });
    tracing::info!(?baseline_sources, "using baseline sources");
    let univ2_sources = baseline_sources
        .iter()
        .filter_map(|source: &BaselineSource| {
            UniV2BaselineSourceParameters::from_baseline_source(*source, &network)
        })
        .chain(args.shared.custom_univ2_baseline_sources.iter().copied());
    let (pair_providers, pool_fetchers): (Vec<_>, Vec<_>) = futures::stream::iter(univ2_sources)
        .then(|source: UniV2BaselineSourceParameters| {
            let web3 = &web3;
            async move {
                let source = source.into_source(web3).await.unwrap();
                (source.pair_provider, source.pool_fetching)
            }
        })
        .unzip()
        .await;

    let base_tokens = Arc::new(BaseTokens::new(
        native_token.address(),
        &args.shared.base_tokens,
    ));
    let mut allowed_tokens = args.allowed_tokens.clone();
    allowed_tokens.extend(base_tokens.tokens().iter().copied());
    allowed_tokens.push(model::order::BUY_ETH_ADDRESS);
    let unsupported_tokens = args.unsupported_tokens.clone();

    let finder = token_owner_finder::init(
        &args.token_owner_finder,
        web3.clone(),
        chain_id,
        &http_factory,
        &pair_providers,
        vault.as_ref(),
        uniswapv3_factory.as_ref(),
        &base_tokens,
    )
    .await
    .expect("failed to initialize token owner finders");

    let trace_call_detector = args.tracing_node_url.as_ref().map(|tracing_node_url| {
        Box::new(CachingDetector::new(
            Box::new(TraceCallDetector {
                web3: shared::ethrpc::web3(
                    &args.shared.ethrpc,
                    &http_factory,
                    tracing_node_url,
                    "trace",
                ),
                finder,
                settlement_contract: settlement_contract.address(),
            }),
            args.token_quality_cache_expiry,
        ))
    });
    let bad_token_detector = Arc::new(
        ListBasedDetector::new(
            allowed_tokens,
            unsupported_tokens,
            trace_call_detector
                .map(|detector| UnknownTokenStrategy::Forward(detector))
                .unwrap_or(UnknownTokenStrategy::Allow),
        )
        .instrumented(),
    );

    let pool_aggregator = PoolAggregator { pool_fetchers };

    let cache_config = CacheConfig {
        number_of_blocks_to_cache: args.shared.pool_cache_blocks,
        number_of_entries_to_auto_update: args.pool_cache_lru_size,
        maximum_recent_block_age: args.shared.pool_cache_maximum_recent_block_age,
        max_retries: args.shared.pool_cache_maximum_retries,
        delay_between_retries: args.shared.pool_cache_delay_between_retries,
    };
    let pool_fetcher = Arc::new(
        PoolCache::new(
            cache_config,
            Arc::new(pool_aggregator),
            current_block_stream.clone(),
        )
        .expect("failed to create pool cache"),
    );
    let block_retriever = args.shared.current_block.retriever(web3.clone());
    let token_info_fetcher = Arc::new(CachedTokenInfoFetcher::new(Arc::new(TokenInfoFetcher {
        web3: web3.clone(),
    })));
    let balancer_pool_fetcher = if baseline_sources.contains(&BaselineSource::BalancerV2) {
        let factories = args
            .shared
            .balancer_factories
            .clone()
            .unwrap_or_else(|| BalancerFactoryKind::for_chain(chain_id));
        let contracts = BalancerContracts::new(&web3, factories).await.unwrap();
        match BalancerPoolFetcher::new(
            &args.shared.graph_api_base_url,
            chain_id,
            block_retriever.clone(),
            token_info_fetcher.clone(),
            cache_config,
            current_block_stream.clone(),
            http_factory.create(),
            web3.clone(),
            &contracts,
            args.shared.balancer_pool_deny_list.clone(),
        )
        .await
        {
            Ok(fetcher) => Some(Arc::new(fetcher)),
            Err(err) => {
                tracing::error!(
                    "failed to create BalancerV2 pool fetcher, this is most likely due to \
                     temporary issues with the graph (in that case consider manually restarting \
                     services once the graph is back online): {:?}",
                    err
                );
                None
            }
        }
    } else {
        None
    };
    let uniswap_v3_pool_fetcher = if baseline_sources.contains(&BaselineSource::UniswapV3) {
        match UniswapV3PoolFetcher::new(
            &args.shared.graph_api_base_url,
            chain_id,
            web3.clone(),
            http_factory.create(),
            block_retriever,
            args.shared.max_pools_to_initialize_cache,
        )
        .await
        {
            Ok(fetcher) => Some(Arc::new(fetcher)),
            Err(err) => {
                tracing::error!(
                    "failed to create UniswapV3 pool fetcher, this is most likely due to \
                     temporary issues with the graph (in that case consider manually restarting \
                     services once the graph is back online): {:?}",
                    err
                );
                None
            }
        }
    } else {
        None
    };
    let block_retriever = args.shared.current_block.retriever(web3.clone());
    let zeroex_api = Arc::new(
        DefaultZeroExApi::new(
            &http_factory,
            args.shared
                .zeroex_url
                .as_deref()
                .unwrap_or(DefaultZeroExApi::DEFAULT_URL),
            args.shared.zeroex_api_key.clone(),
            current_block_stream.clone(),
        )
        .unwrap(),
    );
    let one_inch_api = OneInchClientImpl::new(
        args.shared.one_inch_url.clone(),
        http_factory.create(),
        chain_id,
        current_block_stream.clone(),
    )
    .map(Arc::new);

    let mut price_estimator_factory = PriceEstimatorFactory::new(
        &args.price_estimation,
        &args.shared,
        factory::Network {
            web3: web3.clone(),
            simulation_web3,
            name: network_name.to_string(),
            chain_id,
            native_token: native_token.address(),
            settlement: settlement_contract.address(),
            authenticator: settlement_contract
                .authenticator()
                .call()
                .await
                .expect("failed to query solver authenticator address"),
            base_tokens: base_tokens.clone(),
            block_stream: current_block_stream.clone(),
        },
        factory::Components {
            http_factory: http_factory.clone(),
            bad_token_detector: bad_token_detector.clone(),
            uniswap_v2_pools: pool_fetcher.clone(),
            balancer_pools: balancer_pool_fetcher.clone().map(|a| a as _),
            uniswap_v3_pools: uniswap_v3_pool_fetcher.clone().map(|a| a as _),
            tokens: token_info_fetcher.clone(),
            gas_price: gas_price_estimator.clone(),
            zeroex: zeroex_api.clone(),
            oneinch: one_inch_api.ok().map(|a| a as _),
        },
    )
    .expect("failed to initialize price estimator factory");

    let native_price_estimator = price_estimator_factory
        .native_price_estimator(
            args.native_price_estimators.as_slice(),
            &PriceEstimatorSource::for_args(
                args.order_quoting.price_estimators.as_slice(),
                &args.order_quoting.price_estimation_drivers,
                &args.order_quoting.price_estimation_legacy_solvers,
            ),
            args.native_price_estimation_results_required,
        )
        .unwrap();
    let price_estimator = price_estimator_factory
        .price_estimator(
            &PriceEstimatorSource::for_args(
                args.order_quoting.price_estimators.as_slice(),
                &args.order_quoting.price_estimation_drivers,
                &args.order_quoting.price_estimation_legacy_solvers,
            ),
            native_price_estimator.clone(),
            gas_price_estimator.clone(),
        )
        .unwrap();

    let skip_event_sync_start = if args.skip_event_sync {
        block_number_to_block_number_hash(&web3, BlockNumber::Latest).await
    } else {
        None
    };
    let event_updater = Arc::new(EventUpdater::new(
        GPv2SettlementContract::new(settlement_contract.clone()),
        db.clone(),
        block_retriever.clone(),
        skip_event_sync_start,
    ));
    let mut maintainers: Vec<Arc<dyn Maintaining>> = vec![event_updater, Arc::new(db.clone())];

    let gas_price_estimator = Arc::new(InstrumentedGasEstimator::new(
        shared::gas_price_estimation::create_priority_estimator(
            &http_factory,
            &web3,
            args.shared.gas_estimators.as_slice(),
            args.shared.blocknative_api_key.clone(),
        )
        .await
        .expect("failed to create gas price estimator"),
    ));
    let liquidity_order_owners: HashSet<_> = args
        .order_quoting
        .liquidity_order_owners
        .iter()
        .copied()
        .collect();
    let fee_subsidy = Arc::new(FeeSubsidyConfiguration {
        fee_discount: args.order_quoting.fee_discount,
        min_discounted_fee: args.order_quoting.min_discounted_fee,
        fee_factor: args.order_quoting.fee_factor,
        liquidity_order_owners: liquidity_order_owners.clone(),
    }) as Arc<dyn FeeSubsidizing>;

    let quoter = Arc::new(OrderQuoter::new(
        price_estimator,
        native_price_estimator.clone(),
        gas_price_estimator,
        fee_subsidy,
        Arc::new(db.clone()),
        order_quoting::Validity {
            eip1271_onchain_quote: chrono::Duration::from_std(
                args.order_quoting.eip1271_onchain_quote_validity,
            )
            .unwrap(),
            presign_onchain_quote: chrono::Duration::from_std(
                args.order_quoting.presign_onchain_quote_validity,
            )
            .unwrap(),
            standard_quote: chrono::Duration::from_std(
                args.order_quoting.standard_offchain_quote_validity,
            )
            .unwrap(),
        },
    ));

    if let Some(ethflow_contract) = args.ethflow_contract {
        let start_block = determine_ethflow_indexing_start(
            &skip_event_sync_start,
            args.ethflow_indexing_start,
            &web3,
            chain_id,
        )
        .await;

        let refund_event_handler = Arc::new(
            EventUpdater::new_skip_blocks_before(
                // This cares only about ethflow refund events because all the other ethflow
                // events are already indexed by the OnchainOrderParser.
                EthFlowRefundRetriever::new(web3.clone(), ethflow_contract),
                db.clone(),
                block_retriever.clone(),
                start_block,
            )
            .await
            .unwrap(),
        );
        maintainers.push(refund_event_handler);

        let custom_ethflow_order_parser = EthFlowOnchainOrderParser {};
        let onchain_order_event_parser = OnchainOrderParser::new(
            db.clone(),
            web3.clone(),
            quoter.clone(),
            Box::new(custom_ethflow_order_parser),
            DomainSeparator::new(chain_id, settlement_contract.address()),
            settlement_contract.address(),
            liquidity_order_owners,
        );
        let broadcaster_event_updater = Arc::new(
            EventUpdater::new_skip_blocks_before(
                // The events from the ethflow contract are read with the more generic contract
                // interface called CoWSwapOnchainOrders.
                CoWSwapOnchainOrdersContract::new(web3.clone(), ethflow_contract),
                onchain_order_event_parser,
                block_retriever,
                start_block,
            )
            .await
            .expect("Should be able to initialize event updater. Database read issues?"),
        );
        maintainers.push(broadcaster_event_updater);
    }
    if let Some(uniswap_v3) = uniswap_v3_pool_fetcher {
        maintainers.push(uniswap_v3);
    }

    let service_maintainer = ServiceMaintenance::new(maintainers);
    tokio::task::spawn(
        service_maintainer.run_maintenance_on_new_block(current_block_stream.clone()),
    );

    let block = current_block_stream.borrow().number;
    let solvable_orders_cache = SolvableOrdersCache::new(
        args.min_order_validity_period,
        db.clone(),
        args.banned_users.iter().copied().collect(),
        balance_fetcher.clone(),
        bad_token_detector.clone(),
        current_block_stream.clone(),
        native_price_estimator.clone(),
        signature_validator.clone(),
        args.auction_update_interval,
        args.ethflow_contract,
        native_token.address(),
        args.limit_order_price_factor
            .try_into()
            .expect("limit order price factor can't be converted to BigDecimal"),
    );
    solvable_orders_cache
        .update(block)
        .await
        .expect("failed to perform initial solvable orders update");
    let liveness = Liveness {
        max_auction_age: args.max_auction_age,
        solvable_orders_cache: solvable_orders_cache.clone(),
    };
    shared::metrics::serve_metrics(Arc::new(liveness), args.metrics_address);

    let on_settlement_event_updater =
        crate::on_settlement_event_updater::OnSettlementEventUpdater {
            web3: web3.clone(),
            contract: settlement_contract,
            native_token: native_token.address(),
            db: db.clone(),
        };
    tokio::task::spawn(
        on_settlement_event_updater
            .run_forever(current_block_stream.clone())
            .instrument(tracing::info_span!("on_settlement_event_updater")),
    );

    let order_events_cleaner_config = crate::periodic_db_cleanup::OrderEventsCleanerConfig::new(
        args.order_events_cleanup_interval,
        args.order_events_cleanup_threshold,
    );
    let order_events_cleaner = crate::periodic_db_cleanup::OrderEventsCleaner::new(
        order_events_cleaner_config,
        db.clone(),
    );

    tokio::task::spawn(
        order_events_cleaner
            .run_forever()
            .instrument(tracing::info_span!("order_events_cleaner")),
    );

    let market_makable_token_list_configuration = TokenListConfiguration {
        url: args.trusted_tokens_url,
        update_interval: args.trusted_tokens_update_interval,
        chain_id,
        client: http_factory.create(),
        hardcoded: args.trusted_tokens.unwrap_or_default(),
    };
    // updated in background task
    let market_makable_token_list =
        AutoUpdatingTokenList::from_configuration(market_makable_token_list_configuration).await;

    let ethrpc = ethrpc(&args.shared.node_url).await;
    let eth = ethereum(ethrpc).await;
    let run = RunLoop {
        eth,
        solvable_orders_cache,
        database: Arc::new(db),
        drivers: args.drivers.into_iter().map(Driver::new).collect(),
        market_makable_token_list,
        submission_deadline: args.submission_deadline as u64,
        additional_deadline_for_rewards: args.additional_deadline_for_rewards as u64,
        score_cap: args.score_cap,
        max_settlement_transaction_wait: args.max_settlement_transaction_wait,
        solve_deadline: args.solve_deadline,
        in_flight_orders: Default::default(),
        fee_policy: args.fee_policy,
        persistence: infra::persistence::Persistence::new(args.s3.into().unwrap()).await,
    };
    run.run_forever().await;
    unreachable!("run loop exited");
}

async fn shadow_mode(args: Arguments) -> ! {
    let http_factory = HttpClientFactory::new(&args.http_client);

    let orderbook = protocol::Orderbook::new(
        http_factory.create(),
        args.shadow.expect("missing shadow mode configuration"),
    );

    let drivers = args.drivers.into_iter().map(Driver::new).collect();

    let trusted_tokens = {
        let web3 = shared::ethrpc::web3(
            &args.shared.ethrpc,
            &http_factory,
            &args.shared.node_url,
            "base",
        );

        let chain_id = web3
            .eth()
            .chain_id()
            .await
            .expect("Could not get chainId")
            .as_u64();
        if let Some(expected_chain_id) = args.shared.chain_id {
            assert_eq!(
                chain_id, expected_chain_id,
                "connected to node with incorrect chain ID",
            );
        }

        AutoUpdatingTokenList::from_configuration(TokenListConfiguration {
            url: args.trusted_tokens_url,
            update_interval: args.trusted_tokens_update_interval,
            chain_id,
            client: http_factory.create(),
            hardcoded: args.trusted_tokens.unwrap_or_default(),
        })
        .await
    };

    shared::metrics::serve_metrics(Arc::new(shadow::Liveness), args.metrics_address);

    let shadow = shadow::RunLoop::new(
        orderbook,
        drivers,
        trusted_tokens,
        args.score_cap,
        args.solve_deadline,
        args.fee_policy,
    );
    shadow.run_forever().await;

    unreachable!("shadow run loop exited");
}
