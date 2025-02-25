use {
    crate::{
        liquidity::slippage,
        s3_instance_upload_arguments::S3UploadArguments,
        settlement_access_list::AccessListEstimatorType,
        solver::{
            risk_computation,
            single_order_solver,
            ExternalSolverArg,
            SolverAccountArg,
            SolverType,
        },
    },
    ethcontract::U256,
    primitive_types::H160,
    reqwest::Url,
    shared::{
        arguments::{display_list, display_option},
        http_client,
    },
    std::time::Duration,
};

#[derive(clap::Parser)]
pub struct Arguments {
    #[clap(flatten)]
    pub shared: shared::arguments::Arguments,

    #[clap(flatten)]
    pub http_client: http_client::Arguments,

    #[clap(flatten)]
    pub slippage: slippage::Arguments,

    #[clap(flatten)]
    pub order_prioritization: single_order_solver::Arguments,

    /// The API endpoint to fetch the orderbook
    #[clap(long, env, default_value = "http://localhost:8080")]
    pub orderbook_url: Url,

    /// The API endpoint to call the Quasimodo solver
    #[clap(long, env, default_value = "http://localhost:8000")]
    pub quasimodo_solver_url: Url,

    /// The API endpoint for the Balancer SOR API for solving.
    #[clap(long, env, default_value = "http://localhost:8000")]
    pub balancer_sor_url: Url,

    /// The account used by the driver to sign transactions. This can be either
    /// a 32-byte private key for offline signing, a 20-byte Ethereum address
    /// for signing with a local node account, or a KMS key ID for signing with
    /// AWS.
    #[clap(long, env, hide_env_values = true)]
    pub solver_account: Option<SolverAccountArg>,

    /// The target confirmation time in seconds for settlement transactions used
    /// to estimate gas price.
    #[clap(
        long,
        env,
        default_value = "30s",
        value_parser = humantime::parse_duration,
    )]
    pub target_confirm_time: Duration,

    /// Specify the interval between consecutive driver run loops.
    ///
    /// This is typically a low value to prevent busy looping in case of some
    /// internal driver error, but can be set to a larger value for running
    /// drivers in dry-run mode to prevent repeatedly settling the same
    /// orders.
    #[clap(
        long,
        env,
        default_value = "10s",
        value_parser = humantime::parse_duration,
    )]
    pub settle_interval: Duration,

    /// Which type of solver to use
    #[clap(
        long,
        env,
        default_values = &["Naive", "Baseline"],
        value_enum,
        ignore_case = true,
        use_value_delimiter = true
    )]
    pub solvers: Vec<SolverType>,

    /// Individual accounts for each solver. See `--solver-account` for more
    /// information about configuring accounts.
    #[clap(
        long,
        env,
        ignore_case = true,
        use_value_delimiter = true,
        hide_env_values = true
    )]
    pub solver_accounts: Option<Vec<SolverAccountArg>>,

    /// List of external solvers in the form of `name|url|account`.
    #[clap(long, env, use_value_delimiter = true)]
    pub external_solvers: Option<Vec<ExternalSolverArg>>,

    /// The port at which we serve our metrics
    #[clap(long, env, default_value = "9587")]
    pub metrics_port: u16,

    /// The port at which we serve our metrics
    #[clap(long, env, default_value = "5")]
    pub max_merged_settlements: usize,

    /// The maximum amount of time in seconds a solver is allowed to take.
    #[clap(
        long,
        env,
        default_value = "30s",
        value_parser = humantime::parse_duration,
    )]
    pub solver_time_limit: Duration,

    /// The URL of a list of tokens our settlement contract is willing to buy
    /// when settling trades without external liquidity
    #[clap(long, env)]
    pub market_makable_token_list: Option<Url>,

    /// Like `market_makable_token_list` but hardcoded list of tokens.
    #[clap(long, env, use_value_delimiter = true)]
    pub market_makable_tokens: Option<Vec<H160>>,

    /// Time interval after which market makable list needs to be updated
    #[clap(
        long,
        env,
        default_value = "1h",
        value_parser = humantime::parse_duration,
    )]
    pub market_makable_token_list_update_interval: Duration,

    /// The maximum gas price in Gwei the solver is willing to pay in a
    /// settlement.
    #[clap(
        long,
        env,
        default_value = "1500",
        value_parser = shared::arguments::wei_from_gwei
    )]
    pub gas_price_cap: f64,

    /// How to to submit settlement transactions.
    /// Expected to contain either:
    /// 1. One value equal to TransactionStrategyArg::DryRun or
    /// 2. One or more values equal to any combination of enum variants except
    /// TransactionStrategyArg::DryRun
    #[clap(
        long,
        env,
        default_value = "PublicMempool",
        value_enum,
        ignore_case = true,
        use_value_delimiter = true
    )]
    pub transaction_strategy: Vec<TransactionStrategyArg>,

    /// Which access list estimators to use. Multiple estimators are used in
    /// sequence if a previous one fails. Individual estimators might
    /// support different networks. `Tenderly`: supports every network.
    /// `Web3`: supports every network.
    #[clap(long, env, value_enum, ignore_case = true, use_value_delimiter = true)]
    pub access_list_estimators: Vec<AccessListEstimatorType>,

    /// The API endpoint of the Eden network for transaction submission.
    #[clap(long, env, default_value = "https://api.edennetwork.io/v1/rpc")]
    pub eden_api_url: Url,

    /// The API endpoint of the Flashbots network for transaction submission.
    /// Multiple values could be defined for different Flashbots endpoints
    /// (Flashbots Protect and Flashbots fast).
    #[clap(
        long,
        env,
        use_value_delimiter = true,
        default_value = "https://rpc.flashbots.net"
    )]
    pub flashbots_api_url: Vec<Url>,

    /// Configures whether the submission logic is allowed to assume the
    /// submission nodes implement soft cancellations. With soft cancellations a
    /// cancellation transaction doesn't have to get mined to have an effect. On
    /// arrival in the node all pending transactions with the same sender and
    /// nonce will get discarded immediately.
    #[clap(long, env, action = clap::ArgAction::Set, default_value = "false")]
    pub use_soft_cancellations: bool,

    /// Maximum additional tip in gwei that we are willing to give to eden above
    /// regular gas price estimation
    #[clap(
        long,
        env,
        default_value = "3",
        value_parser = shared::arguments::wei_from_gwei
    )]
    pub max_additional_eden_tip: f64,

    /// The maximum time we spend trying to settle a transaction
    /// through the ethereum network before going to back to solving.
    #[clap(
        long,
        env,
        default_value = "2m",
        value_parser = humantime::parse_duration,
    )]
    pub max_submission_time: Duration,

    /// Maximum additional tip in gwei that we are willing to give to flashbots
    /// above regular gas price estimation
    #[clap(
        long,
        env,
        default_value = "3",
        value_parser = shared::arguments::wei_from_gwei
    )]
    pub max_additional_flashbot_tip: f64,

    /// Amount of time to wait before retrying to submit the tx to the ethereum
    /// network
    #[clap(
        long,
        env,
        default_value = "2s",
        value_parser = humantime::parse_duration,
    )]
    pub submission_retry_interval: Duration,

    /// Additional tip in percentage of max_fee_per_gas we are willing to give
    /// to miners above regular gas price estimation
    #[clap(
        long,
        env,
        default_value = "0.05",
        value_parser = shared::arguments::parse_percentage_factor
    )]
    pub additional_tip_percentage: f64,

    /// The RPC endpoints to use for submitting transaction to a custom set of
    /// nodes.
    #[clap(long, env, use_value_delimiter = true)]
    pub transaction_submission_nodes: Vec<Url>,

    /// Additional RPC endpoints that we notify when we submit a transaction to
    /// the network. These endpoints are usually third parties that seek to
    /// be timely informed of a submission. These URLs are expected to
    /// respond to valid RPC requests. however they are not expected to
    /// be available nor we expect that transaction will eventually be mined.
    #[clap(long, env, use_value_delimiter = true)]
    pub transaction_notification_nodes: Vec<Url>,

    /// Don't submit high revert risk (i.e. transactions that interact with
    /// on-chain AMMs) to the public mempool. This can be enabled to avoid
    /// MEV when private transaction submission strategies are available.
    #[clap(long, env)]
    pub disable_high_risk_public_mempool_transactions: bool,

    /// The maximum number of settlements the driver considers per solver.
    #[clap(long, env, default_value = "20")]
    pub max_settlements_per_solver: usize,

    /// The smallest possible amount in Ether to consider for a partial order.
    #[clap(long, env, default_value = "0.01", value_parser = shared::arguments::wei_from_ether)]
    pub smallest_partial_fill: U256,

    /// Factor how much of the WETH buffer should be unwrapped if ETH buffer is
    /// not big enough to settle ETH buy orders.
    /// Unwrapping a bigger amount will cause fewer unwraps to happen and
    /// thereby reduce the cost of unwraps per settled batch.
    /// Only values in the range [0.0, 1.0] make sense.
    #[clap(long, env, default_value = "0.6", value_parser = shared::arguments::parse_percentage_factor)]
    pub weth_unwrap_factor: f64,

    /// Gas limit for simulations. This parameter is important to set correctly,
    /// such that there are no simulation errors due to: err: insufficient
    /// funds for gas * price + value, but at the same time we don't
    /// restrict solutions sizes too much
    #[clap(long, env, default_value = "15000000")]
    pub simulation_gas_limit: u128,

    /// In order to protect against malicious solvers, the driver will check
    /// that settlements prices do not exceed a max price deviation compared
    /// to the external prices of the driver, if this optional value is set.
    /// The max deviation value should be provided as a float percentage value.
    /// E.g. for a max price deviation of 3%, one should set it to 0.03f64
    #[clap(long, env)]
    pub max_settlement_price_deviation: Option<f64>,

    /// This variable allows to restrict the set of tokens for which a price
    /// deviation check of settlement prices and external prices is
    /// executed. If the value is not set, then all tokens included
    /// in the settlement are checked for price deviation.
    #[clap(long, env, use_value_delimiter = true)]
    pub token_list_restriction_for_price_checks: Option<Vec<H160>>,

    #[clap(flatten)]
    pub s3_upload: S3UploadArguments,

    /// Additional time to wait for a transaction to appear onchain before
    /// considering the solution invalid and setting the reward to 0.
    #[clap(
        long,
        env,
        default_value = "1m",
        value_parser = humantime::parse_duration,
    )]
    pub additional_mining_deadline: Duration,

    /// Parameters used to calculate the success/revert posibility of a
    /// settlement. Currently used for gnosis solvers.
    #[clap(flatten)]
    pub risk_params: risk_computation::Arguments,

    /// Cap used for CIP20 score calculation. Defaults to 0.01 ETH.
    #[clap(long, env, default_value = "0.01", value_parser = shared::arguments::wei_from_ether)]
    pub score_cap: U256,

    /// Should we skip settlements with non-positive score for solver
    /// competition?
    #[clap(long, env, action = clap::ArgAction::Set, default_value = "true")]
    pub skip_non_positive_score_settlements: bool,

    /// Flag to enable RFQ-T liquidity in the 0x solver.
    #[clap(long, env, action = clap::ArgAction::Set, default_value = "false")]
    pub zeroex_enable_rfqt: bool,

    /// Flag to enable slippage protection for the 0x solver.
    #[clap(long, env, action = clap::ArgAction::Set, default_value = "false")]
    pub zeroex_enable_slippage_protection: bool,

    #[clap(long, env, action = clap::ArgAction::Set, default_value = "true")]
    pub process_partially_fillable_liquidity_orders: bool,

    #[clap(long, env, action = clap::ArgAction::Set, default_value = "true")]
    pub process_partially_fillable_limit_orders: bool,

    /// Address of the ETH flow contract. If not specified, eth-flow orders are
    /// disabled.
    #[clap(long, env)]
    pub ethflow_contract: Option<H160>,

    /// Controls whether we discard solutions without a fee for partially
    /// filllable limit orders or set the fee to 0. This can make sense on
    /// chains where we are not so concerned about the fee (e.g. gc,
    /// goerli).
    #[clap(long, env, action = clap::ArgAction::Set, default_value = "true")]
    pub enforce_correct_fees_for_partially_fillable_limit_orders: bool,
}

impl std::fmt::Display for Arguments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.shared)?;
        write!(f, "{}", self.http_client)?;
        write!(f, "{}", self.slippage)?;
        write!(f, "{}", self.order_prioritization)?;
        writeln!(f, "orderbook_url: {}", self.orderbook_url)?;
        writeln!(f, "quasimodo_solver_url: {}", self.quasimodo_solver_url)?;
        writeln!(f, "balancer_sor_url: {}", self.balancer_sor_url)?;
        display_option(
            f,
            "solver_account",
            &self
                .solver_account
                .as_ref()
                .map(|account| format!("{account:?}")),
        )?;
        writeln!(f, "target_confirm_time: {:?}", self.target_confirm_time)?;
        writeln!(f, "settle_interval: {:?}", self.settle_interval)?;
        writeln!(f, "solvers: {:?}", self.solvers)?;
        writeln!(f, "solver_accounts: {:?}", self.solver_accounts)?;
        display_list(
            f,
            "external_solvers",
            self.external_solvers
                .iter()
                .flatten()
                .map(|solver| format!("{}|{}|{:?}", solver.name, solver.url, solver.account)),
        )?;
        writeln!(f, "metrics_port: {}", self.metrics_port)?;
        writeln!(f, "max_merged_settlements: {}", self.max_merged_settlements)?;
        writeln!(f, "solver_time_limit: {:?}", self.solver_time_limit)?;
        display_option(
            f,
            "market_makable_token_list",
            &self.market_makable_token_list,
        )?;
        display_option(
            f,
            "market_makable_tokens",
            &self
                .market_makable_tokens
                .as_ref()
                .map(|list| format!("{list:?}")),
        )?;
        writeln!(f, "gas_price_cap: {}", self.gas_price_cap)?;
        writeln!(f, "transaction_strategy: {:?}", self.transaction_strategy)?;
        writeln!(
            f,
            "access_list_estimators: {:?}",
            &self.access_list_estimators
        )?;
        writeln!(f, "eden_api_url: {}", self.eden_api_url)?;
        display_list(f, "flashbots_api_url", &self.flashbots_api_url)?;
        writeln!(f, "use_soft_cancellations: {}", self.use_soft_cancellations)?;
        writeln!(
            f,
            "max_additional_eden_tip: {}",
            self.max_additional_eden_tip
        )?;
        writeln!(f, "max_submission_time: {:?}", self.max_submission_time)?;
        writeln!(
            f,
            "max_additional_flashbots_tip: {}",
            self.max_additional_flashbot_tip
        )?;
        writeln!(
            f,
            "submission_retry_interval: {:?}",
            self.submission_retry_interval
        )?;
        writeln!(
            f,
            "additional_tip_percentage: {}%",
            self.additional_tip_percentage
        )?;
        display_list(
            f,
            "transaction_submission_nodes",
            &self.transaction_submission_nodes,
        )?;
        display_list(
            f,
            "submission_notification_nodes",
            &self.transaction_notification_nodes,
        )?;
        writeln!(
            f,
            "disable_high_risk_public_mempool_transactions: {}",
            self.disable_high_risk_public_mempool_transactions,
        )?;
        writeln!(
            f,
            "max_settlements_per_solver: {}",
            self.max_settlements_per_solver
        )?;
        writeln!(f, "weth_unwrap_factor: {}", self.weth_unwrap_factor)?;
        writeln!(f, "simulation_gas_limit: {}", self.simulation_gas_limit)?;
        display_option(
            f,
            "max_settlement_price_deviation",
            &self.max_settlement_price_deviation,
        )?;
        writeln!(
            f,
            "token_list_restriction_for_price_checks: {:?}",
            self.token_list_restriction_for_price_checks
        )?;
        writeln!(f, "{}", self.s3_upload)?;
        writeln!(
            f,
            "additional_mining_deadline: {:?}",
            self.additional_mining_deadline
        )?;
        writeln!(f, "{}", self.risk_params)?;
        writeln!(f, "score_cap {}", self.score_cap)?;
        writeln!(f, "{}", self.skip_non_positive_score_settlements)?;
        writeln!(f, "zeroex_enable_rfqt: {}", self.zeroex_enable_rfqt)?;
        writeln!(
            f,
            "zeroex_enable_slippage_protection: {}",
            self.zeroex_enable_slippage_protection
        )?;
        writeln!(
            f,
            "process_partially_fillable_limit_orders: {:?}",
            self.process_partially_fillable_limit_orders
        )?;
        writeln!(
            f,
            "process_partially_fillable_liquidity_orders: {:?}",
            self.process_partially_fillable_liquidity_orders
        )?;
        display_option(f, "ethflow_contract", &self.ethflow_contract)?;
        writeln!(
            f,
            "enforce_correct_fees_for_partially_fillable_limit_orders: {:?}",
            self.enforce_correct_fees_for_partially_fillable_limit_orders
        )?;
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, clap::ValueEnum)]
#[clap(rename_all = "verbatim")]
pub enum TransactionStrategyArg {
    PublicMempool,
    Eden,
    Flashbots,
    DryRun,
}
