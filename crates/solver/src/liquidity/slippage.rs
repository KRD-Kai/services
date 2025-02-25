//! Module defining slippage computation for AMM liquidity.

use {
    super::{AmmOrderExecution, LimitOrder},
    crate::solver::SolverType,
    anyhow::{anyhow, Context as _, Result},
    clap::{Parser, ValueEnum as _},
    ethcontract::{H160, U256},
    model::order::OrderKind,
    num::{BigInt, BigRational, CheckedDiv, Integer as _, ToPrimitive as _},
    once_cell::sync::OnceCell,
    shared::{external_prices::ExternalPrices, http_solver::model::TokenAmount},
    std::{
        borrow::Cow,
        cmp,
        collections::HashMap,
        fmt::{self, Display, Formatter},
        str::FromStr,
    },
};

/// Slippage configuration command line arguments.
#[derive(Debug, Parser)]
#[group(skip)]
pub struct Arguments {
    /// The relative slippage tolerance to apply to on-chain swaps. This flag
    /// expects a comma-separated list of relative slippage values in basis
    /// points per solver. If a solver is not included, it will use the default
    /// global value. For example, "10,oneinch=20,zeroex=5" will configure all
    /// solvers to have 10 BPS of relative slippage tolerance, with 1Inch and
    /// 0x solvers configured for 20 and 5 BPS respectively. The global value
    /// can be specified as `~` to keep it its default. For example,
    /// "~,paraswap=42" will configure all solvers to use the default
    /// configuration, while overriding the ParaSwap solver to use 42 BPS.
    #[clap(long, env, default_value = "10")]
    pub relative_slippage_bps: SlippageArgumentValues<u32>,

    /// The absolute slippage tolerance in native token units to cap relative
    /// slippage at. This makes it so very large trades use a potentially
    /// tighter slippage tolerance to reduce absolute losses. This parameter
    /// uses the same format as `--relative-slippage-bps`. For example,
    /// "~,oneinch=0.001,zeroex=0.042" will disable absolute slippage tolerance
    /// globally for all solvers, while overriding 1Inch and 0x solvers to cap
    /// absolute slippage at 0.001Ξ and 0.042Ξ respectively.
    #[clap(long, env, default_value = "~")]
    pub absolute_slippage_in_native_token: SlippageArgumentValues<f64>,
}

impl Arguments {
    /// Returns the slippage calculator for the specified solver.
    pub fn get_calculator(&self, solver: SolverType) -> SlippageCalculator {
        let bps = self
            .relative_slippage_bps
            .get(solver)
            .copied()
            .unwrap_or(DEFAULT_MAX_SLIPPAGE_BPS);
        let absolute = self
            .absolute_slippage_in_native_token
            .get(solver)
            .map(|value| U256::from_f64_lossy(value * 1e18));

        SlippageCalculator::from_bps(bps, absolute)
    }

    /// Returns the slippage calculator for the specified solver.
    pub fn get_global_calculator(&self) -> SlippageCalculator {
        let bps = self
            .relative_slippage_bps
            .get_global()
            .copied()
            .unwrap_or(DEFAULT_MAX_SLIPPAGE_BPS);
        let absolute = self
            .absolute_slippage_in_native_token
            .get_global()
            .map(|value| U256::from_f64_lossy(value * 1e18));

        SlippageCalculator::from_bps(bps, absolute)
    }
}

impl Display for Arguments {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        writeln!(f, "relative_slippage_bps: {}", self.relative_slippage_bps)?;
        writeln!(
            f,
            "absolute_slippage_in_native_token: {}",
            self.absolute_slippage_in_native_token,
        )?;

        Ok(())
    }
}

/// A comma separated slippage value per solver.
#[derive(Clone, Debug)]
pub struct SlippageArgumentValues<T>(Option<T>, HashMap<SolverType, T>);

impl<T> SlippageArgumentValues<T> {
    /// Gets the slippage configuration value for the specified solver.
    pub fn get(&self, solver: SolverType) -> Option<&T> {
        self.1.get(&solver).or(self.0.as_ref())
    }

    /// Gets the global slippage configuration value.
    pub fn get_global(&self) -> Option<&T> {
        self.0.as_ref()
    }
}

impl<T> Display for SlippageArgumentValues<T>
where
    T: Display,
{
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match &self.0 {
            Some(global) => write!(f, "{global}")?,
            None => f.write_str("~")?,
        }
        for (solver, value) in &self.1 {
            write!(f, ",{solver:?}={value}")?;
        }
        Ok(())
    }
}

impl<T> FromStr for SlippageArgumentValues<T>
where
    T: FromStr,
    anyhow::Error: From<T::Err>,
{
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut values = s.split(',');

        let global_value = values
            .next()
            .map(|value| match value {
                "~" => Ok(None),
                _ => Ok(Some(value.parse()?)),
            })
            .transpose()?
            .flatten();
        let solver_values = values
            .map(|part| {
                let (solver, value) = part
                    .split_once('=')
                    .context("malformed solver slippage value")?;
                Ok((
                    SolverType::from_str(solver, true).map_err(|message| anyhow!(message))?,
                    value.parse()?,
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(Self(global_value, solver_values))
    }
}

/// Constant maximum slippage of 10 BPS (0.1%) to use for on-chain liquidity.
pub const DEFAULT_MAX_SLIPPAGE_BPS: u32 = 10;

/// Basis points in 100%.
const BPS_BASE: u32 = 10000;

/// A per-auction context for computing slippage.
pub struct SlippageContext<'a> {
    prices: &'a ExternalPrices,
    calculator: &'a SlippageCalculator,
}

impl<'a> SlippageContext<'a> {
    /// Returns the external prices used for the slippage context.
    pub fn prices(&self) -> &ExternalPrices {
        self.prices
    }

    /// Computes the slippage amount for the specified token amount.
    pub fn slippage(&self, token: H160, amount: U256) -> Result<SlippageAmount> {
        let (relative, absolute) = self.calculator.compute(
            self.prices.price(&token),
            number::conversions::u256_to_big_int(&amount),
        )?;
        let slippage = SlippageAmount::from_num(&relative, &absolute)?;

        if *relative < self.calculator.relative {
            tracing::debug!(
                ?token,
                %amount,
                relative = ?slippage.relative,
                absolute = %slippage.absolute,
                "reducing relative to respect maximum absolute slippage",
            );
        }

        Ok(slippage)
    }

    /// Applies slippage to the specified AMM execution.
    pub fn apply_to_amm_execution(
        &self,
        mut execution: AmmOrderExecution,
    ) -> Result<AmmOrderExecution> {
        let relative_ratio = |token_amount: &TokenAmount| -> Result<Cow<BigRational>> {
            let (relative, _) = self.calculator.compute(
                self.prices.price(&token_amount.token),
                number::conversions::u256_to_big_int(&token_amount.amount),
            )?;
            Ok(relative)
        };

        // It is possible for AMMs to use tokens that don't have external
        // prices. In order to handle these cases, we do in order:
        // 1. Compute the capped slippage using the sell token amount
        // 2. If no sell token price is available, compute the capped slippage using the
        //    buy token amount
        // 3. Fall back to using the default relative slippage without capping
        let relative = if let Ok(relative) = relative_ratio(&execution.input_max) {
            tracing::debug!(
                input_token = ?execution.input_max.token,
                "using AMM input token for capped surplus",
            );
            relative
        } else if let Ok(relative) = relative_ratio(&execution.output) {
            tracing::debug!(
                output_token = ?execution.output.token,
                "using AMM output token for capped surplus",
            );
            relative
        } else {
            tracing::warn!(
                input_token = ?execution.input_max.token,
                output_token = ?execution.output.token,
                "unable to compute capped slippage; falling back to relative slippage",
            );
            Cow::Borrowed(&self.calculator.relative)
        };

        let absolute = absolute_slippage_amount(
            &relative,
            &number::conversions::u256_to_big_int(&execution.input_max.amount),
        );
        let slippage = SlippageAmount::from_num(&relative, &absolute)?;

        if *relative < self.calculator.relative {
            tracing::debug!(
                input_token = ?execution.input_max.token,
                input_amount = ?execution.input_max.token,
                relative = ?slippage.relative,
                absolute = %slippage.absolute,
                "capping AMM slippage to respect maximum absolute amount",
            );
        }

        execution.input_max.amount = slippage.add_to_amount(execution.input_max.amount);
        Ok(execution)
    }

    /// Applies slippage to an input amount.
    pub fn apply_to_amount_in(&self, token: H160, amount: U256) -> Result<U256> {
        Ok(self.slippage(token, amount)?.add_to_amount(amount))
    }

    /// Applies slippage to an output amount.
    pub fn apply_to_amount_out(&self, token: H160, amount: U256) -> Result<U256> {
        Ok(self.slippage(token, amount)?.sub_from_amount(amount))
    }

    /// Computes the relative slippage for a limit order.
    pub fn relative_for_order(&self, order: &LimitOrder) -> Result<RelativeSlippage> {
        // We use the fixed token and amount for computing relative slippage.
        // This is because the variable token amount may not be representative
        // of the actual trade value. For example, a "pure" market sell order
        // would have almost 0 limit buy amount, which would cause a potentially
        // large order to not get capped on the absolute slippage value.
        let (token, amount) = match order.kind {
            OrderKind::Sell => (order.sell_token, order.sell_amount),
            OrderKind::Buy => (order.buy_token, order.buy_amount),
        };
        self.relative(token, amount)
    }

    /// Computes the relative slippage for a token and amount.
    pub fn relative(&self, token: H160, amount: U256) -> Result<RelativeSlippage> {
        Ok(self.slippage(token, amount)?.relative())
    }
}

impl Default for SlippageContext<'static> {
    fn default() -> Self {
        static CONTEXT: OnceCell<(ExternalPrices, SlippageCalculator)> = OnceCell::new();
        let (prices, calculator) = CONTEXT.get_or_init(Default::default);
        Self { prices, calculator }
    }
}

/// Component used for computing negative slippage limits for internal solvers.
#[derive(Clone, Debug)]
pub struct SlippageCalculator {
    /// The maximum relative slippage factor.
    pub relative: BigRational,
    /// The maximum absolute slippage in native tokens.
    pub absolute: Option<BigInt>,
}

impl SlippageCalculator {
    pub fn from_bps(relative_bps: u32, absolute: Option<U256>) -> Self {
        Self {
            relative: BigRational::new(relative_bps.into(), BPS_BASE.into()),
            absolute: absolute.map(|value| number::conversions::u256_to_big_int(&value)),
        }
    }

    pub fn context<'a>(&'a self, prices: &'a ExternalPrices) -> SlippageContext<'a> {
        SlippageContext {
            prices,
            calculator: self,
        }
    }

    /// Computes the capped slippage amount for the specified token price and
    /// amount.
    pub fn compute(
        &self,
        price: Option<&BigRational>,
        amount: BigInt,
    ) -> Result<(Cow<BigRational>, BigInt)> {
        let relative = if let Some(max_absolute_native_token) = self.absolute.clone() {
            let price = price.context("missing token price")?;
            let max_absolute_slippage = BigRational::new(max_absolute_native_token, 1.into())
                .checked_div(price)
                .context("price is zero")?;

            let amount = BigRational::new(amount.clone(), 1.into());

            let max_relative_slippage_respecting_absolute_limit = max_absolute_slippage
                .checked_div(&amount)
                .context("amount is zero")?;

            cmp::min(
                Cow::Owned(max_relative_slippage_respecting_absolute_limit),
                Cow::Borrowed(&self.relative),
            )
        } else {
            Cow::Borrowed(&self.relative)
        };
        let absolute = absolute_slippage_amount(&relative, &amount);

        Ok((relative, absolute))
    }
}

impl Default for SlippageCalculator {
    fn default() -> Self {
        Self::from_bps(DEFAULT_MAX_SLIPPAGE_BPS, None)
    }
}

/// A result of a slippage computation containing both relative and absolute
/// slippage amounts.
#[derive(Clone, Copy, Debug, Default)]
pub struct SlippageAmount {
    /// The relative slippage amount factor.
    relative: f64,
    /// The absolute slippage amount in the token it was computed for.
    absolute: U256,
}

impl SlippageAmount {
    /// Computes a slippage amount from arbitrary precision `num` values.
    fn from_num(relative: &BigRational, absolute: &BigInt) -> Result<Self> {
        let relative = relative
            .to_f64()
            .context("relative slippage ratio is not a number")?;
        let absolute = number::conversions::big_int_to_u256(absolute)?;

        Ok(Self { relative, absolute })
    }

    /// Reduce the specified amount by the constant slippage.
    pub fn sub_from_amount(&self, amount: U256) -> U256 {
        amount.saturating_sub(self.absolute)
    }

    /// Increase the specified amount by the constant slippage.
    pub fn add_to_amount(&self, amount: U256) -> U256 {
        amount.saturating_add(self.absolute)
    }

    /// Returns the relative slippage value.
    pub fn relative(&self) -> RelativeSlippage {
        RelativeSlippage(self.relative)
    }
}

/// A relative slippage value.
pub struct RelativeSlippage(f64);

impl RelativeSlippage {
    /// Returns the relative slippage as a factor.
    pub fn as_factor(&self) -> f64 {
        self.0
    }

    /// Returns the relative slippage as a percentage.
    pub fn as_percentage(&self) -> f64 {
        self.0 * 100.
    }

    /// Returns the relative slippage as basis points rounded down.
    pub fn as_bps(&self) -> u32 {
        (self.0 * 10000.) as _
    }
}

fn absolute_slippage_amount(relative: &BigRational, amount: &BigInt) -> BigInt {
    let ratio = relative * amount;
    // Perform a ceil division so that we round up with slippage amount
    ratio.numer().div_ceil(ratio.denom())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        shared::{conversions::U256Ext as _, externalprices},
        testlib::tokens::{GNO, USDC, WETH},
    };

    #[test]
    fn limits_max_slippage() {
        let calculator = SlippageCalculator::from_bps(10, Some(U256::exp10(17)));
        let prices = externalprices! {
            native_token: WETH,
            GNO => U256::exp10(9).to_big_rational(),
            USDC => BigRational::new(2.into(), 1000.into()),
        };

        let slippage = calculator.context(&prices);
        for (token, amount, expected_slippage) in [
            (GNO, U256::exp10(12), 1),
            (USDC, U256::exp10(23), 5),
            (GNO, U256::exp10(8), 10),
            (GNO, U256::exp10(17), 0),
        ] {
            let relative = slippage.relative(token, amount).unwrap();
            assert_eq!(relative.as_bps(), expected_slippage);
        }
    }

    #[test]
    fn errors_on_missing_token_price() {
        let calculator = SlippageCalculator::from_bps(10, Some(1_000.into()));
        assert!(calculator
            .context(&externalprices! { native_token: WETH, })
            .slippage(USDC, 1_000_000.into(),)
            .is_err());
    }

    #[test]
    fn test_out_amount_with_slippage() {
        let slippage = SlippageContext::default();
        for (amount, expected) in [
            (0.into(), 0.into()),
            (100.into(), 99.into()),
            (1000.into(), 999.into()),
            (10000.into(), 9990.into()),
            (10001.into(), 9990.into()),
            (
                U256::MAX,
                U256::from_dec_str(
                    "115676297148078879228147414023679219945\
                     416714680974923475418126423905216510295",
                )
                .unwrap(),
            ),
        ] {
            let amount_with_slippage = slippage.apply_to_amount_out(WETH, amount).unwrap();
            assert_eq!(amount_with_slippage, expected);
        }
    }

    #[test]
    fn test_in_amount_with_slippage() {
        let slippage = SlippageContext::default();
        for (amount, expected) in [
            (0.into(), 0.into()),
            (100.into(), 101.into()),
            (1000.into(), 1001.into()),
            (10000.into(), 10010.into()),
            (10001.into(), 10012.into()),
            (U256::MAX, U256::MAX),
        ] {
            let amount_with_slippage = slippage.apply_to_amount_in(WETH, amount).unwrap();
            assert_eq!(amount_with_slippage, expected);
        }
    }

    #[test]
    fn amm_execution_slippage() {
        let calculator = SlippageCalculator::from_bps(100, Some(U256::exp10(18)));
        let prices = externalprices! { native_token: WETH };

        let slippage = calculator.context(&prices);
        let cases = [
            (
                AmmOrderExecution {
                    input_max: TokenAmount::new(WETH, 1_000_000_000_000_000_000_u128),
                    output: TokenAmount::new(GNO, 10_000_000_000_000_000_000_u128),
                    internalizable: false,
                },
                1_010_000_000_000_000_000_u128.into(),
            ),
            (
                AmmOrderExecution {
                    input_max: TokenAmount::new(GNO, 10_000_000_000_000_000_000_000_u128),
                    output: TokenAmount::new(WETH, 1_000_000_000_000_000_000_000_u128),
                    internalizable: false,
                },
                10_010_000_000_000_000_000_000_u128.into(),
            ),
            (
                AmmOrderExecution {
                    input_max: TokenAmount::new(USDC, 200_000_000_u128),
                    output: TokenAmount::new(GNO, 2_000_000_000_000_000_000_u128),
                    internalizable: false,
                },
                202_000_000_u128.into(),
            ),
        ];

        for (execution, expected) in cases {
            let execution = slippage.apply_to_amm_execution(execution).unwrap();
            assert_eq!(execution.input_max.amount, expected);
        }
    }
}
