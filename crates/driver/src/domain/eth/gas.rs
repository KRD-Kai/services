use {
    super::{Ether, U256},
    bigdecimal::Zero,
    num::zero,
    std::{ops, ops::Add},
};

/// Gas amount in gas units.
///
/// The amount of Ether that is paid in transaction fees is proportional to this
/// amount as well as the transaction's [`EffectiveGasPrice`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Gas(pub U256);

impl From<U256> for Gas {
    fn from(value: U256) -> Self {
        Self(value)
    }
}

impl From<u64> for Gas {
    fn from(value: u64) -> Self {
        Self(value.into())
    }
}

impl From<Gas> for U256 {
    fn from(value: Gas) -> Self {
        value.0
    }
}

impl Add for Gas {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Zero for Gas {
    fn zero() -> Self {
        Self(U256::zero())
    }

    fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
}

/// An EIP-1559 gas price estimate.
///
/// https://eips.ethereum.org/EIPS/eip-1559#specification
#[derive(Debug, Clone, Copy)]
pub struct GasPrice {
    /// The maximum total fee that should be charged.
    pub max: FeePerGas,
    /// The maximum priority fee (i.e. the tip to the block proposer) that
    /// can be charged.
    pub tip: FeePerGas,
    /// The current base gas price that will be charged to all accounts on the
    /// next block.
    pub base: FeePerGas,
}

impl GasPrice {
    /// Returns the estimated [`EffectiveGasPrice`] for the gas price estimate.
    pub fn effective(&self) -> EffectiveGasPrice {
        U256::from(self.max)
            .min(U256::from(self.base).saturating_add(self.tip.into()))
            .into()
    }
}

impl From<EffectiveGasPrice> for GasPrice {
    fn from(value: EffectiveGasPrice) -> Self {
        let value = value.0 .0;
        Self {
            max: value.into(),
            tip: value.into(),
            base: value.into(),
        }
    }
}

/// The amount of ETH to pay as fees for a single unit of gas. This is
/// `{max,max_priority,base}_fee_per_gas` as defined by EIP-1559.
///
/// https://eips.ethereum.org/EIPS/eip-1559#specification
#[derive(Debug, Clone, Copy)]
pub struct FeePerGas(pub Ether);

impl From<U256> for FeePerGas {
    fn from(value: U256) -> Self {
        Self(value.into())
    }
}

impl From<FeePerGas> for U256 {
    fn from(value: FeePerGas) -> Self {
        value.0.into()
    }
}

impl ops::Mul<FeePerGas> for Gas {
    type Output = Ether;

    fn mul(self, rhs: FeePerGas) -> Self::Output {
        (self.0 * rhs.0 .0).into()
    }
}

/// The `effective_gas_price` as defined by EIP-1559.
///
/// https://eips.ethereum.org/EIPS/eip-1559#specification
#[derive(Debug, Clone, Copy)]
pub struct EffectiveGasPrice(pub Ether);

impl From<U256> for EffectiveGasPrice {
    fn from(value: U256) -> Self {
        Self(value.into())
    }
}

impl From<EffectiveGasPrice> for U256 {
    fn from(value: EffectiveGasPrice) -> Self {
        value.0.into()
    }
}

impl Add for EffectiveGasPrice {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Zero for EffectiveGasPrice {
    fn zero() -> Self {
        Self(Ether::zero())
    }

    fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
}

impl ops::Mul<EffectiveGasPrice> for Gas {
    type Output = GasCost;

    fn mul(self, rhs: EffectiveGasPrice) -> Self::Output {
        GasCost::new(self, rhs)
    }
}

/// Gas cost in Ether.
///
/// The amount of Ether that is paid in transaction fees.
#[derive(Clone, Copy)]
pub struct GasCost {
    gas: Gas,
    price: EffectiveGasPrice,
}

impl GasCost {
    pub fn new(gas: Gas, price: EffectiveGasPrice) -> Self {
        Self { gas, price }
    }

    pub fn get(&self) -> Ether {
        (self.gas.0 * self.price.0 .0).into()
    }

    pub fn zero() -> Self {
        Self {
            gas: zero(),
            price: zero(),
        }
    }
}

impl std::fmt::Debug for GasCost {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("GasCost")
            .field("gas", &self.gas.0)
            .field("price", &self.price.0 .0)
            .field("gas_cost", &self.get().0)
            .finish()
    }
}
