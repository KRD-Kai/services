use {
    super::single_order_solver::{
        execution_respects_order,
        SettlementError,
        SingleOrderSettlement,
        SingleOrderSolving,
    },
    crate::{
        interactions::allowances::{AllowanceManager, AllowanceManaging, ApprovalRequest},
        liquidity::{slippage::SlippageCalculator, LimitOrder},
    },
    anyhow::{anyhow, Result},
    contracts::GPv2Settlement,
    derivative::Derivative,
    ethcontract::{Account, H160},
    ethrpc::current_block::CurrentBlockStream,
    model::order::OrderKind,
    reqwest::Client,
    shared::{
        ethrpc::Web3,
        external_prices::ExternalPrices,
        paraswap_api::{
            DefaultParaswapApi,
            ParaswapApi,
            ParaswapResponseError,
            PriceQuery,
            PriceResponse,
            Side,
            TradeAmount,
            TransactionBuilderQuery,
        },
        token_info::{TokenInfo, TokenInfoFetching},
    },
    std::{collections::HashMap, sync::Arc},
};

const REFERRER: &str = "GPv2";

/// A GPv2 solver that matches GP orders to direct ParaSwap swaps.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct ParaswapSolver {
    account: Account,
    settlement_contract: GPv2Settlement,
    #[derivative(Debug = "ignore")]
    token_info: Arc<dyn TokenInfoFetching>,
    #[derivative(Debug = "ignore")]
    allowance_fetcher: Box<dyn AllowanceManaging>,
    #[derivative(Debug = "ignore")]
    client: Box<dyn ParaswapApi + Send + Sync>,
    disabled_paraswap_dexs: Vec<String>,
    slippage_calculator: SlippageCalculator,
}

impl ParaswapSolver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        account: Account,
        web3: Web3,
        settlement_contract: GPv2Settlement,
        token_info: Arc<dyn TokenInfoFetching>,
        disabled_paraswap_dexs: Vec<String>,
        client: Client,
        partner: Option<String>,
        base_url: String,
        slippage_calculator: SlippageCalculator,
        block_stream: CurrentBlockStream,
    ) -> Self {
        let allowance_fetcher = AllowanceManager::new(web3.clone(), settlement_contract.address());

        Self {
            account,
            settlement_contract,
            token_info,
            allowance_fetcher: Box::new(allowance_fetcher),
            client: Box::new(DefaultParaswapApi {
                client,
                base_url,
                partner: partner.unwrap_or_else(|| REFERRER.into()),
                block_stream,
            }),
            disabled_paraswap_dexs,
            slippage_calculator,
        }
    }
}

impl From<ParaswapResponseError> for SettlementError {
    fn from(err: ParaswapResponseError) -> Self {
        match err {
            err @ ParaswapResponseError::Request(_) | err @ ParaswapResponseError::Retryable(_) => {
                Self::Retryable(anyhow!(err))
            }
            ParaswapResponseError::RateLimited => Self::RateLimited,
            ParaswapResponseError::InsufficientLiquidity(_) => Self::Benign(anyhow!(err)),
            err => Self::Other(anyhow!(err)),
        }
    }
}

#[async_trait::async_trait]
impl SingleOrderSolving for ParaswapSolver {
    async fn try_settle_order(
        &self,
        order: LimitOrder,
        external_prices: &ExternalPrices,
        _gas_price: f64,
    ) -> Result<Option<SingleOrderSettlement>, SettlementError> {
        let token_info = self
            .token_info
            .get_token_infos(&[order.sell_token, order.buy_token])
            .await;
        let price_response = self.get_price_for_order(&order, &token_info).await?;
        if !execution_respects_order(
            &order,
            price_response.src_amount,
            price_response.dest_amount,
        ) {
            tracing::debug!("execution does not respect order");
            return Ok(None);
        }
        let transaction_query =
            self.transaction_query_from(external_prices, &order, &price_response, &token_info)?;
        let transaction = self.client.transaction(transaction_query, true).await?;
        let mut settlement = SingleOrderSettlement {
            sell_token_price: price_response.dest_amount,
            buy_token_price: price_response.src_amount,
            interactions: Vec::new(),
            executed_amount: order.full_execution_amount(),
            order: order.clone(),
        };
        if let Some(approval) = self
            .allowance_fetcher
            .get_approval(&ApprovalRequest {
                token: order.sell_token,
                spender: price_response.token_transfer_proxy,
                amount: price_response.src_amount,
            })
            .await?
        {
            settlement.interactions.push(Arc::new(approval));
        }
        settlement.interactions.push(Arc::new(transaction));
        Ok(Some(settlement))
    }

    fn account(&self) -> &Account {
        &self.account
    }

    fn name(&self) -> &'static str {
        "ParaSwap"
    }
}

impl ParaswapSolver {
    async fn get_price_for_order(
        &self,
        order: &LimitOrder,
        token_info: &HashMap<H160, TokenInfo>,
    ) -> Result<PriceResponse, ParaswapResponseError> {
        let (amount, side) = match order.kind {
            model::order::OrderKind::Buy => (order.buy_amount, Side::Buy),
            model::order::OrderKind::Sell => (order.sell_amount, Side::Sell),
        };

        let price_query = PriceQuery {
            src_token: order.sell_token,
            dest_token: order.buy_token,
            src_decimals: decimals(token_info, &order.sell_token)?,
            dest_decimals: decimals(token_info, &order.buy_token)?,
            amount,
            side,
            exclude_dexs: Some(self.disabled_paraswap_dexs.clone()),
        };
        let price_response = self.client.price(price_query, true).await?;
        Ok(price_response)
    }

    fn transaction_query_from(
        &self,
        external_prices: &ExternalPrices,
        order: &LimitOrder,
        price_response: &PriceResponse,
        token_info: &HashMap<H160, TokenInfo>,
    ) -> Result<TransactionBuilderQuery> {
        let slippage = self.slippage_calculator.context(external_prices);
        let trade_amount = match order.kind {
            OrderKind::Sell => TradeAmount::Exact {
                src_amount: price_response.src_amount,
                dest_amount: slippage
                    .apply_to_amount_out(order.buy_token, price_response.dest_amount)?,
            },
            OrderKind::Buy => TradeAmount::Exact {
                src_amount: slippage
                    .apply_to_amount_in(order.sell_token, price_response.src_amount)?,
                dest_amount: price_response.dest_amount,
            },
        };
        let query = TransactionBuilderQuery {
            src_token: order.sell_token,
            dest_token: order.buy_token,
            trade_amount,
            src_decimals: decimals(token_info, &order.sell_token)?,
            dest_decimals: decimals(token_info, &order.buy_token)?,
            price_route: price_response.clone().price_route_raw,
            user_address: self.account.address(),
        };
        Ok(query)
    }
}

fn decimals(token_info: &HashMap<H160, TokenInfo>, token: &H160) -> Result<u8> {
    token_info
        .get(token)
        .and_then(|info| info.decimals)
        .ok_or_else(|| anyhow!("decimals for token {:?} not found", token))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            interactions::allowances::{Approval, MockAllowanceManaging},
            test::account,
        },
        contracts::{dummy_contract, WETH9},
        ethcontract::U256,
        ethrpc::current_block::BlockInfo,
        futures::FutureExt as _,
        maplit::hashmap,
        mockall::{predicate::*, Sequence},
        model::order::{Order, OrderData, OrderKind},
        reqwest::Client,
        shared::{
            ethrpc::create_env_test_transport,
            paraswap_api::MockParaswapApi,
            token_info::{MockTokenInfoFetching, TokenInfo, TokenInfoFetcher},
        },
        std::collections::HashMap,
        tokio::sync::watch,
    };

    #[tokio::test]
    async fn test_skips_order_if_unable_to_fetch_decimals() {
        let client = Box::new(MockParaswapApi::new());
        let allowance_fetcher = Box::new(MockAllowanceManaging::new());
        let mut token_info = MockTokenInfoFetching::new();

        token_info
            .expect_get_token_infos()
            .return_const(HashMap::new());

        let solver = ParaswapSolver {
            account: account(),
            client,
            token_info: Arc::new(token_info),
            allowance_fetcher,
            settlement_contract: dummy_contract!(GPv2Settlement, H160::zero()),
            disabled_paraswap_dexs: vec![],
            slippage_calculator: Default::default(),
        };

        let order = LimitOrder::default();
        let result = solver
            .try_settle_order(order, &Default::default(), 1.)
            .await;

        // This implicitly checks that we don't call the API is its mock doesn't have
        // any expectations and would panic
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_respects_limit_price() {
        let mut client = Box::new(MockParaswapApi::new());
        let mut allowance_fetcher = Box::new(MockAllowanceManaging::new());
        let mut token_info = MockTokenInfoFetching::new();

        let sell_token = H160::from_low_u64_be(1);
        let buy_token = H160::from_low_u64_be(2);

        client.expect_price().returning(|_, _| {
            async {
                Ok(PriceResponse {
                    price_route_raw: Default::default(),
                    src_amount: 100.into(),
                    dest_amount: 99.into(),
                    token_transfer_proxy: H160([0x42; 20]),
                    gas_cost: 0,
                })
            }
            .boxed()
        });
        client
            .expect_transaction()
            .returning(|_, _| async { Ok(Default::default()) }.boxed());

        allowance_fetcher
            .expect_get_approval()
            .returning(|_| Ok(None));

        token_info.expect_get_token_infos().returning(move |_| {
            hashmap! {
                sell_token => TokenInfo { decimals: Some(18), symbol: None },
                buy_token => TokenInfo { decimals: Some(18), symbol: None },
            }
        });

        let solver = ParaswapSolver {
            account: account(),
            client,
            token_info: Arc::new(token_info),
            allowance_fetcher,
            settlement_contract: dummy_contract!(GPv2Settlement, H160::zero()),
            disabled_paraswap_dexs: vec![],
            slippage_calculator: Default::default(),
        };

        let order_passing_limit = LimitOrder {
            sell_token,
            buy_token,
            sell_amount: 100.into(),
            buy_amount: 90.into(),
            kind: model::order::OrderKind::Sell,
            ..Default::default()
        };
        let order_violating_limit = LimitOrder {
            sell_token,
            buy_token,
            sell_amount: 100.into(),
            buy_amount: 110.into(),
            kind: model::order::OrderKind::Sell,
            ..Default::default()
        };

        let result = solver
            .try_settle_order(order_passing_limit, &Default::default(), 1.)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.sell_token_price, 99.into());
        assert_eq!(result.buy_token_price, 100.into());

        let result = solver
            .try_settle_order(order_violating_limit, &Default::default(), 1.)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_sets_allowance_if_necessary() {
        let mut client = Box::new(MockParaswapApi::new());
        let mut allowance_fetcher = Box::new(MockAllowanceManaging::new());
        let mut token_info = MockTokenInfoFetching::new();

        let sell_token = H160::from_low_u64_be(1);
        let buy_token = H160::from_low_u64_be(2);
        let token_transfer_proxy = H160([0x42; 20]);

        client.expect_price().returning(move |_, _| {
            async move {
                Ok(PriceResponse {
                    price_route_raw: Default::default(),
                    src_amount: 100.into(),
                    dest_amount: 99.into(),
                    token_transfer_proxy,
                    gas_cost: 0,
                })
            }
            .boxed()
        });
        client
            .expect_transaction()
            .returning(|_, _| async { Ok(Default::default()) }.boxed());

        // On first invocation no prior allowance, then max allowance set.
        let mut seq = Sequence::new();
        allowance_fetcher
            .expect_get_approval()
            .times(1)
            .with(eq(ApprovalRequest {
                token: sell_token,
                spender: token_transfer_proxy,
                amount: U256::from(100),
            }))
            .returning(move |_| {
                Ok(Some(Approval {
                    token: sell_token,
                    spender: token_transfer_proxy,
                }))
            })
            .in_sequence(&mut seq);
        allowance_fetcher
            .expect_get_approval()
            .times(1)
            .with(eq(ApprovalRequest {
                token: sell_token,
                spender: token_transfer_proxy,
                amount: U256::from(100),
            }))
            .returning(|_| Ok(None))
            .in_sequence(&mut seq);

        token_info.expect_get_token_infos().returning(move |_| {
            hashmap! {
                sell_token => TokenInfo { decimals: Some(18), symbol: None },
                buy_token => TokenInfo { decimals: Some(18), symbol: None },
            }
        });

        let solver = ParaswapSolver {
            account: account(),
            client,
            token_info: Arc::new(token_info),
            allowance_fetcher,
            settlement_contract: dummy_contract!(GPv2Settlement, H160::zero()),
            disabled_paraswap_dexs: vec![],
            slippage_calculator: Default::default(),
        };

        let order = LimitOrder {
            sell_token,
            buy_token,
            sell_amount: 100.into(),
            buy_amount: 90.into(),
            ..Default::default()
        };

        // On first run we have two main interactions (approve + swap)
        let result = solver
            .try_settle_order(order.clone(), &Default::default(), 1.)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.interactions.len(), 2);

        // On second run we have only have one main interactions (swap)
        let result = solver
            .try_settle_order(order, &Default::default(), 1.)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.interactions.len(), 1)
    }

    #[tokio::test]
    async fn test_sets_slippage() {
        let mut client = Box::new(MockParaswapApi::new());
        let mut allowance_fetcher = Box::new(MockAllowanceManaging::new());
        let mut token_info = MockTokenInfoFetching::new();

        let sell_token = H160::from_low_u64_be(1);
        let buy_token = H160::from_low_u64_be(2);

        client.expect_price().returning(|_, _| {
            async {
                Ok(PriceResponse {
                    price_route_raw: Default::default(),
                    src_amount: 100.into(),
                    dest_amount: 99.into(),
                    token_transfer_proxy: H160([0x42; 20]),
                    gas_cost: 0,
                })
            }
            .boxed()
        });

        // Check slippage is applied to PriceResponse
        let mut seq = Sequence::new();
        client
            .expect_transaction()
            .times(1)
            .returning(|transaction, _| {
                assert_eq!(
                    transaction.trade_amount,
                    TradeAmount::Exact {
                        src_amount: 100.into(),
                        dest_amount: 89.into(), // 99 - 10% slippage
                    }
                );
                async { Ok(Default::default()) }.boxed()
            })
            .in_sequence(&mut seq);
        client
            .expect_transaction()
            .times(1)
            .returning(|transaction, _| {
                assert_eq!(
                    transaction.trade_amount,
                    TradeAmount::Exact {
                        src_amount: 110.into(), // 100 + 10% slippage
                        dest_amount: 99.into(),
                    }
                );
                async { Ok(Default::default()) }.boxed()
            })
            .in_sequence(&mut seq);

        allowance_fetcher
            .expect_get_approval()
            .returning(|_| Ok(None));

        token_info.expect_get_token_infos().returning(move |_| {
            hashmap! {
                sell_token => TokenInfo { decimals: Some(18), symbol: None },
                buy_token => TokenInfo { decimals: Some(18), symbol: None },
            }
        });

        let solver = ParaswapSolver {
            account: account(),
            client,
            token_info: Arc::new(token_info),
            allowance_fetcher,
            settlement_contract: dummy_contract!(GPv2Settlement, H160::zero()),
            disabled_paraswap_dexs: vec![],
            slippage_calculator: SlippageCalculator::from_bps(1000, None),
        };

        let sell_order = LimitOrder {
            sell_token,
            buy_token,
            sell_amount: 100.into(),
            buy_amount: 90.into(),
            kind: model::order::OrderKind::Sell,
            ..Default::default()
        };

        let result = solver
            .try_settle_order(sell_order, &Default::default(), 1.)
            .await
            .unwrap();
        // Actual assertion is inside the client's `expect_transaction` mock
        assert!(result.is_some());

        let buy_order = LimitOrder {
            sell_token,
            buy_token,
            sell_amount: 100.into(),
            buy_amount: 90.into(),
            kind: model::order::OrderKind::Buy,
            ..Default::default()
        };
        let result = solver
            .try_settle_order(buy_order, &Default::default(), 1.)
            .await
            .unwrap();
        // Actual assertion is inside the client's `expect_transaction` mock
        assert!(result.is_some());
    }

    #[tokio::test]
    #[ignore]
    async fn solve_order_on_paraswap() {
        let web3 = Web3::new(create_env_test_transport());
        let settlement = GPv2Settlement::deployed(&web3).await.unwrap();
        let token_info_fetcher = Arc::new(TokenInfoFetcher { web3: web3.clone() });

        let weth = WETH9::deployed(&web3).await.unwrap();
        let gno = testlib::tokens::GNO;
        let (_, block_stream) = watch::channel(BlockInfo::default());

        let solver = ParaswapSolver::new(
            account(),
            web3,
            settlement,
            token_info_fetcher,
            vec![],
            Client::new(),
            None,
            "https://apiv5.paraswap.io".into(),
            SlippageCalculator::default(),
            block_stream,
        );

        let settlement = solver
            .try_settle_order(
                Order {
                    data: OrderData {
                        sell_token: weth.address(),
                        buy_token: gno,
                        sell_amount: 1_000_000_000_000_000_000u128.into(),
                        buy_amount: 1u128.into(),
                        kind: OrderKind::Sell,
                        ..Default::default()
                    },
                    ..Default::default()
                }
                .into(),
                &Default::default(),
                1.,
            )
            .await
            .unwrap()
            .unwrap();

        println!("{settlement:#?}");
    }
}
