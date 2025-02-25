//! Framework for setting up tests.

use {
    self::{blockchain::Fulfillment, driver::Driver, solver::Solver},
    crate::{
        domain::{competition::order, eth},
        infra::time,
        tests::{
            cases::{
                AB_ORDER_AMOUNT,
                CD_ORDER_AMOUNT,
                DEFAULT_POOL_AMOUNT_A,
                DEFAULT_POOL_AMOUNT_B,
                DEFAULT_POOL_AMOUNT_C,
                DEFAULT_POOL_AMOUNT_D,
                DEFAULT_SCORE_MAX,
                DEFAULT_SCORE_MIN,
                DEFAULT_SURPLUS_FACTOR,
                ETH_ORDER_AMOUNT,
            },
            setup::blockchain::Blockchain,
        },
        util::{self, serialize},
    },
    ethcontract::BlockId,
    hyper::StatusCode,
    secp256k1::SecretKey,
    serde_with::serde_as,
    std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
        str::FromStr,
    },
    web3::types::TransactionId,
};

mod blockchain;
mod driver;
mod solver;

#[derive(Debug, Clone, Copy)]
pub struct Asset {
    token: &'static str,
    amount: eth::U256,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Partial {
    #[default]
    No,
    Yes {
        executed: eth::U256,
    },
}

#[serde_as]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum Score {
    Solver {
        #[serde_as(as = "serialize::U256")]
        score: eth::U256,
    },
    #[serde(rename_all = "camelCase")]
    RiskAdjusted { success_probability: f64 },
}

impl Default for Score {
    fn default() -> Self {
        Self::RiskAdjusted {
            success_probability: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub name: &'static str,

    pub sell_amount: eth::U256,
    pub sell_token: &'static str,
    pub buy_token: &'static str,

    pub internalize: bool,
    pub side: order::Side,
    pub partial: Partial,
    pub valid_for: util::Timestamp,
    pub kind: order::Kind,

    pub user_fee: eth::U256,
    // Currently used for limit orders to represent the surplus_fee calculated by the solver.
    pub solver_fee: Option<eth::U256>,

    /// Set a value to be used to divide the order buy or sell amount before
    /// the order gets placed and thereby generate surplus. Whether the sell or
    /// buy amount is divided depends on the order side. This is necessary to
    /// keep the solution scores positive.
    pub surplus_factor: eth::U256,
    /// Override the executed amount of the order. Useful for testing liquidity
    /// orders. Otherwise [`execution_diff`] is probably more suitable.
    pub executed: Option<eth::U256>,

    /// Should this order be filtered out before being sent to the solver?
    pub filtered: bool,
    /// Should the trader account be funded with enough tokens to place this
    /// order? True by default.
    pub funded: bool,
}

impl Order {
    /// Rename the order.
    pub fn rename(self, name: &'static str) -> Self {
        Self { name, ..self }
    }

    /// Reduce the sell amount of this order by the given amount.
    pub fn reduce_amount(self, diff: eth::U256) -> Self {
        Self {
            sell_amount: self.sell_amount - diff,
            ..self
        }
    }

    /// Multiply the sell amount of this order by the given factor.
    pub fn multiply_amount(self, mult: eth::U256) -> Self {
        Self {
            sell_amount: self.sell_amount * mult,
            ..self
        }
    }

    pub fn user_fee(self, amount: eth::U256) -> Self {
        Self {
            user_fee: amount,
            ..self
        }
    }

    /// Ensure that this order generates no surplus, and therefore most likely
    /// has a negative score.
    pub fn no_surplus(self) -> Self {
        Self {
            surplus_factor: 1.into(),
            ..self
        }
    }

    /// Mark this order as internalizable.
    pub fn internalize(self) -> Self {
        Self {
            internalize: true,
            ..self
        }
    }

    /// Set the order kind.
    pub fn kind(self, kind: order::Kind) -> Self {
        Self { kind, ..self }
    }

    /// Set the order side.
    pub fn side(self, side: order::Side) -> Self {
        Self { side, ..self }
    }

    /// Set the solver fee.
    pub fn solver_fee(self, solver_fee: Option<eth::U256>) -> Self {
        Self { solver_fee, ..self }
    }

    /// Make this a limit order.
    pub fn limit(self) -> Self {
        Self {
            kind: order::Kind::Limit,
            ..self
        }
    }

    /// Mark that this order should be filtered out before being sent to the
    /// solver.
    pub fn filtered(self) -> Self {
        Self {
            filtered: true,
            ..self
        }
    }

    /// Mark that the trader should not be funded with tokens that are needed to
    /// place this order.
    pub fn unfunded(self) -> Self {
        Self {
            funded: false,
            ..self
        }
    }

    fn surplus_fee(&self) -> eth::U256 {
        match self.kind {
            order::Kind::Limit => self.solver_fee.unwrap_or_default(),
            _ => 0.into(),
        }
    }
}

impl Default for Order {
    fn default() -> Self {
        Self {
            sell_amount: Default::default(),
            sell_token: Default::default(),
            buy_token: Default::default(),
            internalize: Default::default(),
            side: order::Side::Sell,
            partial: Default::default(),
            valid_for: 100.into(),
            kind: order::Kind::Market,
            user_fee: Default::default(),
            solver_fee: Default::default(),
            name: Default::default(),
            surplus_factor: DEFAULT_SURPLUS_FACTOR.into(),
            executed: Default::default(),
            filtered: Default::default(),
            funded: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub token_a: &'static str,
    pub token_b: &'static str,
    pub amount_a: eth::U256,
    pub amount_b: eth::U256,
}

/// Create a builder for the setup process.
pub fn setup() -> Setup {
    Setup {
        name: Default::default(),
        pools: Default::default(),
        orders: Default::default(),
        trusted: Default::default(),
        config_file: Default::default(),
        solutions: Default::default(),
        quote: Default::default(),
        fund_solver: true,
        enable_simulation: true,
        settlement_address: Default::default(),
    }
}

#[derive(Debug)]
pub struct Setup {
    name: Option<String>,
    pools: Vec<blockchain::Pool>,
    orders: Vec<Order>,
    trusted: HashSet<&'static str>,
    config_file: Option<PathBuf>,
    solutions: Vec<Solution>,
    /// Is this a test for the /quote endpoint?
    quote: bool,
    /// Should the solver be funded with ETH? True by default.
    fund_solver: bool,
    /// Should simulation be enabled? True by default.
    enable_simulation: bool,
    /// Ensure the settlement contract is deployed on a specific address?
    settlement_address: Option<eth::H160>,
}

/// The validity of a solution.
#[derive(Debug, Clone, Copy)]
pub enum Calldata {
    /// Set up the solver to return a solution with valid calldata.
    Valid {
        /// Include additional meaningless bytes appended to the calldata. This
        /// is useful for lowering the solution score in a controlled
        /// way.
        additional_bytes: usize,
    },
    /// Set up the solver to return a solution with bogus calldata.
    Invalid,
}

#[derive(Debug, Clone)]
pub struct Solution {
    pub calldata: Calldata,
    pub orders: Vec<&'static str>,
    pub score: Score,
}

impl Solution {
    /// Reduce the solution score by adding additional meaningless calldata.
    pub fn reduce_score(self) -> Self {
        Self {
            calldata: match self.calldata {
                Calldata::Valid { .. } => Calldata::Valid {
                    additional_bytes: 10,
                },
                Calldata::Invalid => Calldata::Invalid,
            },
            ..self
        }
    }

    /// Make the solution return invalid calldata.
    pub fn invalid(self) -> Self {
        Self {
            calldata: Calldata::Invalid,
            ..self
        }
    }

    /// Set the solution score to the specified value.
    pub fn score(self, score: Score) -> Self {
        Self { score, ..self }
    }
}

impl Default for Solution {
    fn default() -> Self {
        Self {
            calldata: Calldata::Valid {
                additional_bytes: 0,
            },
            orders: Default::default(),
            score: Default::default(),
        }
    }
}

/// A pool between tokens "A" and "B".
pub fn ab_pool() -> Pool {
    Pool {
        token_a: "A",
        token_b: "B",
        amount_a: DEFAULT_POOL_AMOUNT_A.into(),
        amount_b: DEFAULT_POOL_AMOUNT_B.into(),
    }
}

/// An example order which sells token "A" for token "B".
pub fn ab_order() -> Order {
    Order {
        name: "A-B order",
        sell_amount: AB_ORDER_AMOUNT.into(),
        sell_token: "A",
        buy_token: "B",
        ..Default::default()
    }
}

/// A solution solving the [`ab_order`].
pub fn ab_solution() -> Solution {
    Solution {
        calldata: Calldata::Valid {
            additional_bytes: 0,
        },
        orders: vec!["A-B order"],
        score: Default::default(),
    }
}

/// A pool between tokens "C" and "D".
pub fn cd_pool() -> Pool {
    Pool {
        token_a: "C",
        token_b: "D",
        amount_a: DEFAULT_POOL_AMOUNT_C.into(),
        amount_b: DEFAULT_POOL_AMOUNT_D.into(),
    }
}

/// An example order which sells token "C" for token "D".
pub fn cd_order() -> Order {
    Order {
        name: "C-D order",
        sell_amount: CD_ORDER_AMOUNT.into(),
        sell_token: "C",
        buy_token: "D",
        ..Default::default()
    }
}

/// A solution solving the [`cd_order`].
pub fn cd_solution() -> Solution {
    Solution {
        calldata: Calldata::Valid {
            additional_bytes: 0,
        },
        orders: vec!["C-D order"],
        score: Default::default(),
    }
}

/// A pool between "A" and "WETH".
pub fn weth_pool() -> Pool {
    Pool {
        token_a: "A",
        token_b: "WETH",
        amount_a: DEFAULT_POOL_AMOUNT_A.into(),
        amount_b: DEFAULT_POOL_AMOUNT_B.into(),
    }
}

/// An order which buys ETH.
pub fn eth_order() -> Order {
    Order {
        name: "ETH order",
        sell_amount: ETH_ORDER_AMOUNT.into(),
        sell_token: "A",
        buy_token: "ETH",
        ..Default::default()
    }
}

pub fn eth_solution() -> Solution {
    Solution {
        calldata: Calldata::Valid {
            additional_bytes: 0,
        },
        orders: vec!["ETH order"],
        score: Default::default(),
    }
}

impl Setup {
    /// Set an explicit name for this test. If a name is set, it will be logged
    /// before the test runs.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Add a uniswap pool with the specified reserves. Tokens are identified
    /// by their symbols. Every order will be solved through one of the pools.
    pub fn pool(mut self, pool: Pool) -> Self {
        self.pools.push(blockchain::Pool {
            reserve_a: Asset {
                token: pool.token_a,
                amount: pool.amount_a,
            },
            reserve_b: Asset {
                token: pool.token_b,
                amount: pool.amount_b,
            },
        });
        self
    }

    /// Add a new order to be solved as part of the test. This order will be
    /// passed to /solve when [`Test::solve`] is called and it will be
    /// anticipated by the mock solver.
    pub fn order(mut self, order: Order) -> Self {
        self.orders.push(order);
        self
    }

    /// Set up the protocol to consider the specified token as trusted. The
    /// token is identified by its symbol.
    pub fn trust(mut self, token: &'static str) -> Self {
        self.trusted.insert(token);
        self
    }

    /// Load the specified config file. Otherwise, a temporary config file will
    /// be created with reasonable values.
    pub fn config(mut self, path: PathBuf) -> Self {
        self.config_file = Some(path);
        self
    }

    /// Add a solution to be returned by the mock solver.
    pub fn solution(mut self, solution: Solution) -> Self {
        self.solutions.push(solution);
        self
    }

    /// Don't fund the solver account with any ETH.
    pub fn defund_solver(mut self) -> Self {
        self.fund_solver = false;
        self
    }

    /// Ensure that the settlement contract is deployed to a specific address.
    pub fn settlement_address(mut self, address: &str) -> Self {
        self.settlement_address = Some(address.parse().unwrap());
        self
    }

    /// Create the test: set up onchain contracts and pools, start a mock HTTP
    /// server for the solver and start the HTTP server for the driver.
    pub async fn done(self) -> Test {
        observe::tracing::initialize_reentrant(
            "driver=trace,driver::tests::setup::blockchain=debug",
        );

        if let Some(name) = self.name.as_ref() {
            tracing::warn!("\n***** [RUNNING TEST CASE] *****\n{name}");
        }

        let deadline = self.deadline();
        let Self {
            pools,
            orders,
            trusted,
            config_file,
            ..
        } = self;

        // Hardcoded trader account. Don't use this account for anything else!!!
        let trader_address =
            eth::H160::from_str("d2525C68A663295BBE347B65C87c8e17De936a0a").unwrap();
        let trader_secret_key = SecretKey::from_slice(
            &hex::decode("f9f831cee763ef826b8d45557f0f8677b27045e0e011bcd78571a40acc8a6cc3")
                .unwrap(),
        )
        .unwrap();

        // Hardcoded solver account. Don't use this account for anything else!!!
        let solver_address =
            eth::H160::from_str("72b92Ee5F847FBB0D243047c263Acd40c34A63B9").unwrap();
        let solver_secret_key = SecretKey::from_slice(
            &hex::decode("a131a35fb8f614b31611f4fe68b6fc538b0febd2f75cd68e1282d8fd45b63326")
                .unwrap(),
        )
        .unwrap();

        let relative_slippage = 0.3;
        let absolute_slippage = 183.into();

        // Create the necessary components for testing.
        let blockchain = Blockchain::new(blockchain::Config {
            pools,
            trader_address,
            trader_secret_key,
            solver_address,
            solver_secret_key,
            fund_solver: self.fund_solver,
            settlement_address: self.settlement_address,
        })
        .await;
        let mut solutions = Vec::new();
        for solution in self.solutions {
            let orders = solution
                .orders
                .iter()
                .map(|solution_order| orders.iter().find(|o| o.name == *solution_order).unwrap());
            solutions.push(blockchain.fulfill(orders, &solution).await);
        }
        let mut quotes = Vec::new();
        for order in orders {
            let quote = blockchain.quote(&order).await;
            quotes.push(quote);
        }
        let solver = Solver::new(solver::Config {
            blockchain: &blockchain,
            solutions: &solutions,
            trusted: &trusted,
            quoted_orders: &quotes,
            deadline,
            quote: self.quote,
        })
        .await;
        let driver = Driver::new(
            &driver::Config {
                config_file,
                relative_slippage,
                absolute_slippage,
                solver_address,
                solver_secret_key,
                enable_simulation: self.enable_simulation,
            },
            &solver,
            &blockchain,
        )
        .await;

        Test {
            blockchain,
            driver,
            client: Default::default(),
            trader_address,
            fulfillments: solutions.into_iter().flat_map(|s| s.fulfillments).collect(),
            trusted,
            deadline,
            quoted_orders: quotes,
            quote: self.quote,
        }
    }

    /// This is a test for the /quote endpoint.
    pub fn quote(self) -> Self {
        Self {
            quote: true,
            ..self
        }
    }

    fn deadline(&self) -> chrono::DateTime<chrono::Utc> {
        time::now() + chrono::Duration::seconds(2)
    }
}

pub struct Test {
    quoted_orders: Vec<blockchain::QuotedOrder>,
    blockchain: Blockchain,
    driver: Driver,
    client: reqwest::Client,
    trader_address: eth::H160,
    fulfillments: Vec<Fulfillment>,
    trusted: HashSet<&'static str>,
    deadline: chrono::DateTime<chrono::Utc>,
    /// Is this testing the /quote endpoint?
    quote: bool,
}

impl Test {
    /// Call the /solve endpoint.
    pub async fn solve(&self) -> Solve {
        let res = self
            .client
            .post(format!(
                "http://{}/{}/solve",
                self.driver.addr,
                solver::NAME
            ))
            .json(&driver::solve_req(self))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let body = res.text().await.unwrap();
        tracing::debug!(?status, ?body, "got a response from /solve");
        Solve {
            status,
            body,
            fulfillments: &self.fulfillments,
            blockchain: &self.blockchain,
        }
    }

    /// Call the /reveal endpoint.
    pub async fn reveal(&self) -> Reveal {
        let res = self
            .client
            .post(format!(
                "http://{}/{}/reveal",
                self.driver.addr,
                solver::NAME
            ))
            .json(&driver::reveal_req())
            .send()
            .await
            .unwrap();
        let status = res.status();
        let body = res.text().await.unwrap();
        tracing::debug!(?status, ?body, "got a response from /reveal");
        Reveal { status, body }
    }

    /// Call the /quote endpoint.
    pub async fn quote(&self) -> Quote {
        if !self.quote {
            panic!("called /quote on a test which wasn't configured to test the /quote endpoint");
        }

        let res = self
            .client
            .get(format!(
                "http://{}/{}/quote",
                self.driver.addr,
                solver::NAME
            ))
            .query(&driver::quote_req(self))
            .send()
            .await
            .unwrap();
        let status = res.status();
        let body = res.text().await.unwrap();
        tracing::debug!(?status, ?body, "got a response from /quote");
        Quote {
            fulfillments: &self.fulfillments,
            status,
            body,
        }
    }

    /// Call the /settle endpoint.
    pub async fn settle(&self) -> Settle {
        let old_balances = self.balances().await;
        let old_block = self
            .blockchain
            .web3
            .eth()
            .block_number()
            .await
            .unwrap()
            .as_u64();
        let res = self
            .client
            .post(format!(
                "http://{}/{}/settle",
                self.driver.addr,
                solver::NAME
            ))
            .json(&driver::settle_req())
            .send()
            .await
            .unwrap();
        let status = res.status();
        let body = res.text().await.unwrap();
        tracing::debug!(?status, ?body, "got a response from /settle");
        Settle {
            old_balances,
            old_block,
            status,
            test: self,
            body,
        }
    }

    async fn balances(&self) -> HashMap<&'static str, eth::U256> {
        let mut balances = HashMap::new();
        for (token, contract) in self.blockchain.tokens.iter() {
            let balance = contract
                .balance_of(self.trader_address)
                .call()
                .await
                .unwrap();
            balances.insert(*token, balance);
        }
        balances.insert(
            "WETH",
            self.blockchain
                .weth
                .balance_of(self.trader_address)
                .call()
                .await
                .unwrap(),
        );
        balances.insert(
            "ETH",
            self.blockchain
                .web3
                .eth()
                .balance(self.trader_address, None)
                .await
                .unwrap(),
        );
        balances
    }
}

/// A /solve response.
pub struct Solve<'a> {
    status: StatusCode,
    body: String,
    fulfillments: &'a [Fulfillment],
    blockchain: &'a Blockchain,
}

pub struct SolveOk<'a> {
    body: String,
    fulfillments: &'a [Fulfillment],
    blockchain: &'a Blockchain,
}

impl<'a> Solve<'a> {
    /// Expect the /solve endpoint to have returned a 200 OK response.
    pub fn ok(self) -> SolveOk<'a> {
        assert_eq!(self.status, hyper::StatusCode::OK);
        SolveOk {
            body: self.body,
            fulfillments: self.fulfillments,
            blockchain: self.blockchain,
        }
    }

    pub fn status(self, code: hyper::StatusCode) {
        assert_eq!(self.status, code);
    }
}

impl<'a> SolveOk<'a> {
    fn solutions(&self) -> Vec<serde_json::Value> {
        #[derive(serde::Deserialize)]
        struct Body {
            solutions: Vec<serde_json::Value>,
        }
        serde_json::from_str::<Body>(&self.body).unwrap().solutions
    }

    /// Extracts the first solution from the response. This is expected to be
    /// always valid if there is a valid solution, as we expect from driver to
    /// not send multiple solutions (yet).
    fn solution(&self) -> serde_json::Value {
        let solutions = self.solutions();
        assert_eq!(solutions.len(), 1);
        let solution = solutions[0].clone();
        assert!(solution.is_object());
        assert_eq!(solution.as_object().unwrap().len(), 5);
        solution
    }

    /// Extracts the score from the response. Since response can contain
    /// multiple solutions, it takes the score from the first solution.
    pub fn score(&self) -> eth::U256 {
        let solution = self.solution();
        assert!(solution.get("score").is_some());
        let score = solution.get("score").unwrap().as_str().unwrap();
        eth::U256::from_dec_str(score).unwrap()
    }

    /// Ensure that the score in the response is within a certain range. The
    /// reason why this is a range is because small timing differences in
    /// the test can lead to the settlement using slightly different amounts
    /// of gas, which in turn leads to different scores.
    pub fn score_in_range(self, min: eth::U256, max: eth::U256) -> Self {
        let score = self.score();
        assert!(score >= min, "score less than min {score} < {min}");
        assert!(score <= max, "score more than max {score} > {max}");
        self
    }

    /// Ensure that the score is within the default expected range.
    pub fn default_score(self) -> Self {
        self.score_in_range(DEFAULT_SCORE_MIN.into(), DEFAULT_SCORE_MAX.into())
    }

    /// Ensures that `/solve` returns no solutions.
    pub fn empty(self) {
        assert!(self.solutions().is_empty());
    }

    /// Check that the solution contains the expected orders.
    pub fn orders(self, order_names: &[&str]) -> Self {
        let solution = self.solution();
        assert!(solution.get("orders").is_some());
        let trades = serde_json::from_value::<HashMap<String, serde_json::Value>>(
            solution.get("orders").unwrap().clone(),
        )
        .unwrap();

        for expected in order_names.iter().map(|name| {
            self.fulfillments
                .iter()
                .find(|f| f.quoted_order.order.name == *name)
                .unwrap_or_else(|| {
                    panic!(
                        "unexpected orders {order_names:?}: fulfillment not found in {:?}",
                        self.fulfillments,
                    )
                })
        }) {
            let uid = expected.quoted_order.order_uid(self.blockchain);
            let trade = trades
                .get(&uid.to_string())
                .expect("Didn't find expected trade in solution");
            let u256 = |value: &serde_json::Value| {
                eth::U256::from_dec_str(value.as_str().unwrap()).unwrap()
            };
            assert!(u256(trade.get("buyAmount").unwrap()) == expected.quoted_order.buy);
            assert!(
                u256(trade.get("sellAmount").unwrap())
                    == expected.quoted_order.sell + expected.quoted_order.order.user_fee
            );
        }
        self
    }
}

/// A /reveal response.
pub struct Reveal {
    status: StatusCode,
    body: String,
}

impl Reveal {
    /// Expect the /reveal endpoint to have returned a 200 OK response.
    pub fn ok(self) -> RevealOk {
        assert_eq!(self.status, hyper::StatusCode::OK);
        RevealOk { body: self.body }
    }
}

pub struct RevealOk {
    body: String,
}

impl RevealOk {
    pub fn calldata(self) -> Self {
        let result: serde_json::Value = serde_json::from_str(&self.body).unwrap();
        assert!(result.is_object());
        assert_eq!(result.as_object().unwrap().len(), 1);
        let calldata = result.get("calldata").unwrap().as_object().unwrap();
        assert_eq!(calldata.len(), 2);
        assert!(!calldata
            .get("internalized")
            .unwrap()
            .as_str()
            .unwrap()
            .is_empty());
        assert!(!calldata
            .get("uninternalized")
            .unwrap()
            .as_str()
            .unwrap()
            .is_empty());
        self
    }
}

/// A /quote response.
pub struct Quote<'a> {
    fulfillments: &'a [Fulfillment],
    status: StatusCode,
    body: String,
}

impl<'a> Quote<'a> {
    /// Expect the /quote endpoint to have returned a 200 OK response.
    pub fn ok(self) -> QuoteOk<'a> {
        assert_eq!(self.status, hyper::StatusCode::OK);
        QuoteOk {
            fulfillments: self.fulfillments,
            body: self.body,
        }
    }
}

pub struct QuoteOk<'a> {
    fulfillments: &'a [Fulfillment],
    body: String,
}

impl QuoteOk<'_> {
    /// Check that the quote returns the expected amount of tokens. This is
    /// based on the state of the blockchain and the test setup.
    pub fn amount(self) -> Self {
        assert_eq!(self.fulfillments.len(), 1);
        let fulfillment = &self.fulfillments[0];
        let result: serde_json::Value = serde_json::from_str(&self.body).unwrap();
        let amount = result.get("amount").unwrap().as_str().unwrap().to_owned();
        let expected = match fulfillment.quoted_order.order.side {
            order::Side::Buy => (fulfillment.quoted_order.sell
                - fulfillment.quoted_order.order.surplus_fee())
            .to_string(),
            order::Side::Sell => fulfillment.quoted_order.buy.to_string(),
        };
        assert_eq!(amount, expected);
        self
    }

    /// Check that the quote returns the expected interactions. This is
    /// based on the state of the blockchain and the test setup.
    pub fn interactions(self) -> Self {
        assert_eq!(self.fulfillments.len(), 1);
        let fulfillment = &self.fulfillments[0];
        let result: serde_json::Value = serde_json::from_str(&self.body).unwrap();
        let interactions = result
            .get("interactions")
            .unwrap()
            .as_array()
            .unwrap()
            .to_owned();
        assert_eq!(interactions.len(), fulfillment.interactions.len());
        for (interaction, expected) in interactions.iter().zip(&fulfillment.interactions) {
            let target = interaction.get("target").unwrap().as_str().unwrap();
            let value = interaction.get("value").unwrap().as_str().unwrap();
            let calldata = interaction.get("callData").unwrap().as_str().unwrap();
            assert_eq!(target, format!("0x{}", hex::encode(expected.address)));
            assert_eq!(value, "0");
            assert_eq!(calldata, format!("0x{}", hex::encode(&expected.calldata)));
        }
        self
    }
}

/// The expected difference between a previous user balance for a certain token
/// and the balance after the settlement has been broadcast.
#[derive(Debug, Clone, Copy)]
pub enum Balance {
    /// The balance should be greater than before.
    Greater,
    /// The balance should be smaller than before by an exact amount.
    SmallerBy(eth::U256),
}

/// A /settle response.
pub struct Settle<'a> {
    old_balances: HashMap<&'static str, eth::U256>,
    old_block: u64,
    status: StatusCode,
    test: &'a Test,
    body: String,
}

pub struct SettleOk<'a> {
    test: &'a Test,
    old_balances: HashMap<&'static str, eth::U256>,
}

pub struct SettleErr {
    body: String,
}

impl<'a> Settle<'a> {
    /// Expect the /settle endpoint to have returned a 200 OK response.
    pub async fn ok(self) -> SettleOk<'a> {
        // Ensure that the response is OK.
        assert_eq!(self.status, hyper::StatusCode::OK);
        let result: serde_json::Value = serde_json::from_str(&self.body).unwrap();
        assert!(result.is_object());
        assert_eq!(result.as_object().unwrap().len(), 2);
        assert!(!result
            .get("calldata")
            .unwrap()
            .get("internalized")
            .unwrap()
            .as_str()
            .unwrap()
            .is_empty());
        assert!(!result
            .get("calldata")
            .unwrap()
            .get("uninternalized")
            .unwrap()
            .as_str()
            .unwrap()
            .is_empty());

        let reported_tx_hash =
            serde_json::from_value::<eth::H256>(result.get("txHash").unwrap().clone()).unwrap();

        // Wait for the new block with the settlement to be mined.
        blockchain::wait_for_block(&self.test.blockchain.web3, self.old_block + 1).await;

        // Ensure that the solution ID is included in the settlement.
        let tx = self
            .test
            .blockchain
            .web3
            .eth()
            .transaction(TransactionId::Block(
                BlockId::Number(ethcontract::BlockNumber::Latest),
                0.into(),
            ))
            .await
            .unwrap()
            .unwrap();

        let input = tx.input.0;
        let len = input.len();
        let tx_auction_id = u64::from_be_bytes((&input[len - 8..]).try_into().unwrap());
        assert_eq!(tx_auction_id.to_string(), "1");
        assert_eq!(reported_tx_hash, tx.hash);

        // Ensure that the internalized calldata returned by the driver is equal to the
        // calldata published to the blockchain.
        let internalized = result
            .get("calldata")
            .unwrap()
            .get("internalized")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(
            internalized,
            format!("0x{}", hex::encode(&input).to_lowercase())
        );

        SettleOk {
            test: self.test,
            old_balances: self.old_balances,
        }
    }

    /// Expect the /settle endpoint to return a 400 BAD REQUEST response.
    pub fn err(self) -> SettleErr {
        assert_eq!(self.status, hyper::StatusCode::BAD_REQUEST);
        SettleErr { body: self.body }
    }
}

impl<'a> SettleOk<'a> {
    /// Check that the user balance changed.
    pub async fn balance(self, token: &'static str, balance: Balance) -> SettleOk<'a> {
        let new_balances = self.test.balances().await;
        let new_balance = new_balances.get(token).unwrap();
        let old_balance = self.old_balances.get(token).unwrap();
        match balance {
            Balance::Greater => assert!(new_balance > old_balance),
            Balance::SmallerBy(diff) => assert_eq!(*new_balance, old_balance - diff),
        }
        self
    }

    /// Ensure that the onchain balances changed in accordance with the
    /// [`ab_order`].
    pub async fn ab_order_executed(self) -> SettleOk<'a> {
        self.balance("A", Balance::SmallerBy(AB_ORDER_AMOUNT.into()))
            .await
            .balance("B", Balance::Greater)
            .await
    }

    /// Ensure that the onchain balances changed in accordance with the
    /// [`cd_order`].
    pub async fn cd_order_executed(self) -> SettleOk<'a> {
        self.balance("C", Balance::SmallerBy(CD_ORDER_AMOUNT.into()))
            .await
            .balance("D", Balance::Greater)
            .await
    }

    /// Ensure that the onchain balances changed in accordance with the
    /// [`eth_order`].
    pub async fn eth_order_executed(self) -> SettleOk<'a> {
        self.balance("A", Balance::SmallerBy(ETH_ORDER_AMOUNT.into()))
            .await
            .balance("ETH", Balance::Greater)
            .await
    }
}

impl SettleErr {
    /// Check the kind field in the error response.
    pub fn kind(self, expected_kind: &str) {
        let result: serde_json::Value = serde_json::from_str(&self.body).unwrap();
        assert!(result.is_object());
        assert_eq!(result.as_object().unwrap().len(), 2);
        assert!(result.get("kind").is_some());
        assert!(result.get("description").is_some());
        let kind = result.get("kind").unwrap().as_str().unwrap();
        assert_eq!(kind, expected_kind);
    }
}
