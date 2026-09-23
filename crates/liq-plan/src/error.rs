//! Encode / validate failures. Every variant is fail-closed: the plan is
//! not emitted.

use alloy_primitives::{Address, B256, U256};
use liq_types::FlashProvider;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum EncodeError {
    #[error("plan has zero flash groups")]
    NoGroups,
    #[error("group has zero liquidation legs")]
    NoLegs,
    #[error("group count exceeds u8")]
    TooManyGroups,
    #[error("leg count exceeds u8")]
    TooManyLegs,
    #[error("swap data length {0} exceeds u16")]
    DataTooLong(usize),
    #[error("collateral {collateral} closed by {closers} TAKE_BALANCE legs (need 1)")]
    BadCollateralClosure { collateral: Address, closers: usize },
    #[error("EXACT_OUT leg after TAKE_BALANCE in the same blob")]
    ExactOutAfterTakeBalance,
    #[error("repay swap tokenOut is not the group's debt asset {group}")]
    RepayTargetMismatch { group: Address },
    #[error("profit swap tokenOut is not WETH {weth}")]
    ProfitTargetNotWeth { weth: Address },
    #[error("too many cascade sources for debt {debt}: {n} (cap 3)")]
    TooManySourcesForDebt { debt: Address, n: usize },
    #[error("duplicate provider/source for debt {debt} provider {provider:?}")]
    DuplicateSourceForDebt {
        debt: Address,
        provider: FlashProvider,
    },
    #[error("V4 reserve id {id} on spoke {spoke} is not pinned")]
    V4ReserveUnpinned { spoke: Address, id: u16 },
    #[error("V4 reserve id {id} on spoke {spoke} pins {pinned}, leg has {got}")]
    V4ReserveMismatch {
        spoke: Address,
        id: u16,
        pinned: Address,
        got: Address,
    },
    #[error("V4 leg tail is not two u16 reserve ids")]
    V4TailShape,
    #[error("Morpho market id {id} is not in the registry")]
    MorphoIdUnknown { id: B256 },
    #[error("Morpho id {id} is loan={loan} coll={coll}, leg has debt={debt} coll={got_coll}")]
    MorphoTokenMismatch {
        id: B256,
        loan: Address,
        coll: Address,
        debt: Address,
        got_coll: Address,
    },
    #[error("Morpho id {id} keccak(MarketParams) mismatch")]
    MorphoIdHashMismatch { id: B256 },
    #[error("Morpho leg tail is not a 32-byte Id")]
    MorphoTailShape,
    #[error("V3 leg must have empty tail")]
    V3TailShape,
    #[error("Euler V2 leg tail is not minYieldBalance ‖ collateral vault")]
    EulerTailShape,
    #[error("Euler V2 collateral vault is zero")]
    EulerZeroVault,
    #[error("Silo V2 leg must have empty tail (receiveSToken is hardcoded)")]
    SiloTailShape,
    #[error("Liquity V2 leg tail is not a uint256 troveId")]
    LiquityTailShape,
    #[error("Liquity V2 troveId is zero (EmptyData on-chain)")]
    LiquityZeroTrove,
    #[error("Fluid T1 leg tail is not a uint256 colPerUnitDebt")]
    FluidTailShape,
    #[error("Fluid T1 colPerUnitDebt is zero")]
    FluidZeroColPer,
    #[error("Fluid T1 colPerUnitDebt is 1e27-scale; wire unit is 1e18")]
    FluidColPerNot1e18,
    #[error("Fluid T1 colPerUnitDebt 1e18 conversion failed")]
    FluidColPerConvert,
    #[error("Gearbox leg tail is not a uint256 minSeizedAmount")]
    GearboxTailShape,
    #[error("Gearbox minSeizedAmount is zero")]
    GearboxZeroMinSeized,
    #[error("Compound V2 leg tail is not cTokenCollateral ‖ isCEther")]
    CompoundTailShape,
    #[error("Compound V2 cTokenCollateral is zero")]
    CompoundZeroCToken,
    #[error("Compound V2 isCEther flag is not 0 or 1")]
    CompoundBadFlag,
    #[error("Compound V2 pair debt={market} coll={ctoken_collateral} is not pinned")]
    CompoundUnpinned {
        market: Address,
        ctoken_collateral: Address,
    },
    #[error("Compound V2 market pins {pinned}, leg has {got}")]
    CompoundMarketMismatch { pinned: Address, got: Address },
    #[error("Compound V2 cTokenCollateral pins {pinned}, leg has {got}")]
    CompoundCTokenMismatch { pinned: Address, got: Address },
    #[error("Compound V2 isCEther pins {pinned}, leg has {got}")]
    CompoundCEtherMismatch { pinned: u8, got: u8 },
    #[error("Liquity V2 troveId {trove_id} is not pinned")]
    LiquityUnpinned { trove_id: U256 },
    #[error("Liquity V2 TroveManager pins {pinned}, leg has {got}")]
    LiquityMarketMismatch { pinned: Address, got: Address },
    #[error("Liquity V2 troveId pins {pinned}, leg has {got}")]
    LiquityTroveMismatch { pinned: U256, got: U256 },
    #[error("Liquity V2 borrower pins {pinned}, leg has {got}")]
    LiquityBorrowerMismatch { pinned: Address, got: Address },
    #[error("unknown swap venue {0}")]
    UnknownVenue(u8),
    #[error("UniV3 pool-direct data must be 20 bytes, got {0}")]
    BadPoolDataLen(usize),
    #[error("router data must start with a 20-byte target, got {0}")]
    BadRouterDataLen(usize),
    #[error("zero address in plan field {0}")]
    ZeroAddress(&'static str),
    #[error("protocol_pull {pull} is zero")]
    ZeroPull { pull: u128 },
    #[error("minProfit is zero (dust / multi-leg floor refused)")]
    ZeroMinProfit,
    #[error("EXACT_OUT repay {exact_out} exceeds pull + flash premium {owed}")]
    UnderSeizure { exact_out: u128, owed: u128 },
    #[error("EXACT_OUT repay {exact_out} != pull + flash premium {owed}")]
    RepayNotSizedToPull { exact_out: u128, owed: u128 },
    #[error("flash {flash} is below protocol pull {pull}")]
    FlashShort { flash: u128, pull: u128 },
    #[error("no pool fee for {provider:?} at {fee_bps} bps")]
    UnpriceableFee {
        provider: FlashProvider,
        fee_bps: u16,
    },
    #[error("flash premium does not fit u128")]
    PremiumOverflow,
    #[error("surplus debt {debt} (flash {flash} > pull {pull}) has no TAKE_BALANCE profit leg")]
    SurplusDebtUnrouted {
        debt: Address,
        flash: u128,
        pull: u128,
    },
    #[error("liq-exec wire: {0}")]
    Wire(#[from] liq_exec::wire::WireError),
}

pub type Result<T> = core::result::Result<T, EncodeError>;
