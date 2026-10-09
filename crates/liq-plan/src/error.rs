//! Encode / validate failures. Every variant is fail-closed: the plan is
//! not emitted.

use alloy_primitives::{Address, B256, U256};
use liq_types::FlashProvider;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum EncodeError {
    /// A chain leg (venue 10) is malformed: hop count, length, hop kind,
    /// or not exact output.
    #[error("malformed chain leg: {0}")]
    BadChain(&'static str),
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
    #[error("collateral {collateral} closed by {closers} TAKE_BALANCE legs (need {need})")]
    BadCollateralClosure {
        collateral: Address,
        closers: usize,
        need: usize,
    },
    #[error(
        "a leg spends a set amount of {token} after a TAKE_BALANCE leg on it in the same blob"
    )]
    SpentAfterTakeBalance { token: Address },
    #[error("repay swap tokenOut is neither the group's debt asset {group} nor WETH")]
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
    #[error(
        "Fluid leg tail is not type ‖ flags ‖ colPerUnitDebt ‖ debtPerShareMax ‖ colPerShareMin"
    )]
    FluidTailShape,
    #[error("Fluid colPerUnitDebt is zero")]
    FluidZeroColPer,
    #[error("Fluid vault type {0} is not 1..=4")]
    FluidBadKind(u8),
    #[error("Fluid tail flags {0:#04x} set an undefined bit or a token1 choice on a normal side")]
    FluidBadFlags(u8),
    #[error(
        "Fluid type {0}: a per-share figure is missing on a smart side or set on a normal one"
    )]
    FluidPerShare(u8),
    #[error("Fluid colPerUnitDebt 1e18 conversion failed")]
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
    #[error("UniV3 pool-direct data must be 20 bytes (21 with a factory id), got {0}")]
    BadPoolDataLen(usize),
    #[error("router data must start with a 20-byte target, got {0}")]
    BadRouterDataLen(usize),
    #[error("UniV2 pair data must be 21 bytes (pair ‖ factory id), got {0}")]
    BadV2DataLen(usize),
    #[error("UniV2 factory id {0} is not Uniswap (0) or SushiSwap (1)")]
    BadV2Factory(u8),
    #[error("Balancer pool data must be a 32-byte pool id, got {0}")]
    BadBalancerData(usize),
    #[error("Fluid DEX data must be 21 bytes (pool ‖ swap0to1 0/1), got {0}")]
    BadFluidData(usize),
    /// A venue-0 leg names a V3 factory id that is neither SushiSwap's nor
    /// PancakeSwap's (Uniswap's is the absent byte, never written).
    #[error("V3 pool leg names unknown factory id {0}")]
    BadV3Factory(u8),
    #[error(
        "Curve pool data must be 23 bytes (pool ‖ i ‖ j ‖ MetaRegistry handler index), got {0}"
    )]
    BadCurveDataLen(usize),
    #[error("Curve legs are exact input only")]
    CurveExactOut,
    #[error("unwrap leg data must be the 20-byte vault that is tokenIn, got {0} bytes")]
    BadUnwrapData(usize),
    #[error("unwrap legs are exact input only")]
    UnwrapExactOut,
    #[error("a reward-only group (provider None) must borrow nothing: zero flash amount, source and fee")]
    RewardGroupBorrows,
    #[error("a reward-only group has nothing to repay, so no repay swaps")]
    RewardGroupRepays,
    #[error("a reward-only group's legs pull nothing, got {pull}")]
    RewardGroupPulls { pull: u128 },
    #[error("a flash-swap group holds one liquidation leg, got {legs}")]
    FlashSwapLegs { legs: usize },
    #[error("a flash-swap group's repay legs must not buy the debt {debt}: the lender pool did")]
    FlashSwapRepaysDebt { debt: Address },
    #[error("a flash-swap group's swap leg uses the lender pool {pool}, locked while it swaps")]
    FlashSwapLegOnLender { pool: Address },
    #[error("an unwrap leg must come before every other repay leg")]
    UnwrapNotFirst,
    #[error("unwrapped {asset} is not closed to WETH by a TAKE_BALANCE leg")]
    UnwrapOutputUnclosed { asset: Address },
    #[error("zero address in plan field {0}")]
    ZeroAddress(&'static str),
    #[error("protocol_pull {pull} is zero")]
    ZeroPull { pull: u128 },
    #[error("minProfit is zero (dust / multi-leg floor refused)")]
    ZeroMinProfit,
    #[error("leg {leg}'s EXACT_OUT repay {exact_out} exceeds its pull {pull} (the Executor adds the flash premium)")]
    UnderSeizure {
        leg: usize,
        exact_out: u128,
        pull: u128,
    },
    #[error("leg {leg}'s EXACT_OUT repay {exact_out} != its pull {pull} (the Executor adds the flash premium)")]
    RepayNotSizedToPull {
        leg: usize,
        exact_out: u128,
        pull: u128,
    },
    #[error(
        "leg {leg}'s repay cannot carry the flash premium: no pool exact output, exact input or unwrap into the debt"
    )]
    PremiumUncovered { leg: usize },
    #[error("an exact-output repay leg buys {token}, not the group's debt {debt}: the Executor adds the flash premium to it")]
    ExactOutNotDebt { token: Address, debt: Address },
    #[error("swap leg tied to liquidation leg {tie} of a group of {legs}")]
    TieOutOfRange { tie: usize, legs: usize },
    #[error("a set-amount repay leg on {token} in a group of {legs} legs is not tied to its leg")]
    UntiedRepay { token: Address, legs: usize },
    #[error(
        "a TAKE_BALANCE leg on {token} is tied: it spends what arrived, whichever leg seized it"
    )]
    TiedTakeBalance { token: Address },
    #[error("profit leg on {token} is tied: it runs after every group")]
    TiedProfitLeg { token: Address },
    #[error("a group of {legs} legs ties more than {max} can name")]
    TooManyTiedLegs { legs: usize, max: usize },
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
    #[error(
        "governance flags are set by with_gov_payload / with_gov_spell, not in BatchPlan.flags"
    )]
    GovFlagWithoutPayload,
    #[error("plan already carries a governance action")]
    GovPayloadTwice,
    #[error("spell address is zero")]
    ZeroSpell,
    #[error("governance payload id {0} exceeds uint40")]
    PayloadIdRange(u64),
    #[error("liq-exec wire: {0}")]
    Wire(#[from] liq_wire::wire::WireError),
}

pub type Result<T> = core::result::Result<T, EncodeError>;
