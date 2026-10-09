// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {LiqLeg, FluidTail, PlanDecoder} from "./lib/PlanDecoder.sol";
import {
    IERC20, IWETH, IAavePool, IAaveV4Spoke, IMorpho, MarketParams,
    IEVault, IEVC, ISiloHook, ITroveManager, IFluidT1, IFluidT2, IFluidT3, IFluidT4,
    ICreditFacadeV3, ICreditFacadeV3Multicall, MultiCall, PriceUpdate,
    ICToken, ICErc20, ICEther, IPayloadsController, IDssSpell, IDSPause
} from "./lib/Interfaces.sol";
import {MainnetVenues} from "./lib/MainnetVenues.sol";
import {T_ENTERED, ModuleIds} from "./lib/ExecutorShared.sol";

/*
 * LiquidationModule — the Executor's protocol liquidations and governance
 * actions (GUIDE 10).
 *
 * Code only. The Executor DELEGATECALLs it, so every line here runs as the
 * Executor: its address, balances, allowances and transient storage, and
 * its events. It holds nothing, has no storage, and refuses to run any other
 * way (`onlyDelegated`). Its address is an Executor immutable: a new module
 * is a new Executor (D55).
 */
contract LiquidationModule {
    using SafeTransfer for address;
    using PlanDecoder for bytes;

    /// What the Executor's constructor checks this address answers.
    bytes32 public constant MODULE_ID = ModuleIds.LIQUIDATION;

    /// Canonical WETH. Native ETH a protocol pays out is wrapped into it so
    /// the Executor's profit, measured in WETH, sees it.
    address public immutable WETH;
    /// This contract's own address: running with `address(this) == SELF` is
    /// a direct call, not the Executor's delegatecall.
    address private immutable SELF;

    /// Morpho `SharesMathLib` virtual shares/assets (pin 8e26ca6a).
    uint256 private constant MORPHO_VIRTUAL_SHARES = 1e6;
    uint256 private constant MORPHO_VIRTUAL_ASSETS = 1;

    // `stage` values for `LegFailed`. The first two are reads a leg needs
    // before it can call (Morpho's totals and position, Liquity's trove
    // status, an EVC or credit manager address); the LIQUIDATE stage is the
    // protocol call itself, which is also where a position that is not
    // liquidatable is refused: `reason` then holds the protocol's own error.
    uint8 private constant ST_GUARD      = 1; // a read the leg needs reverted or answered nothing
    uint8 private constant ST_NOT_LIQ    = 2; // that read shows nothing to liquidate
    uint8 private constant ST_LIQUIDATE  = 3; // the liquidation call reverted
    uint8 private constant ST_SIZING     = 4; // pre-call sizing made the leg a no-op
    uint8 private constant ST_TAIL       = 5; // the leg tail is malformed or unset

    // The same declarations as the Executor's, which lists every error its
    // `execute` can surface (`ExecutorModules.t.sol` holds them equal).
    error NotDelegated();
    error ZeroAddress();
    error UnknownAdapter(uint8 a);
    /// A Morpho leg whose market `Id` does not resolve to the encoded
    /// `(loanToken, collateralToken)`. Encoder bug, not a race: revert all.
    error LegMismatch();
    /// Seized cTokens did not redeem. The whole `execute` reverts so the
    /// liquidation and the flash roll back together.
    error RedeemFailed(address token, uint256 code);
    /// Gearbox full liquidation delivered less collateral than the plan's
    /// minimum.
    error SeizedBelowMin(uint256 got, uint256 minimum);

    /// Why a liquidation leg did not fill. Every `catch` here emits one
    /// before returning false, so a skipped leg is diagnosable from the
    /// receipt instead of being indistinguishable from "no opportunity".
    /// `stage` names which call failed; `reason` is the raw revert data,
    /// truncated to the first 256 bytes (a custom-error selector plus args,
    /// or an ABI-encoded `Error(string)`). Emitted as the Executor.
    event LegFailed(
        uint8 indexed adapter,
        address indexed market,
        address indexed borrower,
        uint8 stage,
        bytes reason
    );
    /// `executePayload` reverted inside a governance plan: already executed
    /// (a keeper or an earlier transaction in the bundle), cancelled, or not
    /// yet due. The liquidation legs still run against the resulting state.
    event GovExecSkipped(uint40 indexed payloadId, bytes reason);
    /// A spell plan's `cast()` was not run: DSPause does not hold its plan
    /// (already cast, dropped, or not a scheduled spell; empty reason), or
    /// `cast()` reverted (office hours, not yet due).
    event GovSpellSkipped(address indexed spell, bytes reason);

    constructor(address weth_) {
        if (weth_ == address(0)) revert ZeroAddress();
        WETH = weth_;
        SELF = address(this);
    }

    /// Module code runs only as the Executor's own, inside an `execute`.
    /// Called directly it would run on this contract's empty state: no
    /// funds or allowances are reachable that way, and refusing makes that
    /// argument unnecessary. `T_ENTERED` is read from the caller's transient
    /// storage, which under delegatecall is the Executor's.
    modifier onlyDelegated() {
        uint256 entered;
        assembly { entered := tload(T_ENTERED) }
        if (address(this) == SELF || entered == 0) revert NotDelegated();
        _;
    }

    /// One flash group's liquidation legs, from `liqOffset`. Returns which
    /// filled, one bit per leg (bit i = leg i): the Executor reverts the
    /// group when none did, and the swap module skips every swap leg tied to
    /// a leg that did not.
    ///
    /// Each leg stands or falls alone. A competitor taking one position
    /// between simulation and inclusion must not cost us the others — that
    /// is the whole risk batching introduces, and per-leg tolerance is the
    /// whole mitigation. The profit floor still judges the plan as a whole.
    ///
    /// `payable` because delegatecall keeps the Executor's `msg.value`: a
    /// wallet-funded `execute` would otherwise revert here. No value moves.
    function runLegs(address debtAsset, uint256 liqOffset, uint8 liqCount, bytes calldata plan)
        external payable onlyDelegated returns (uint256 filled)
    {
        uint256 o = liqOffset;
        for (uint256 i; i < liqCount; ++i) {
            (LiqLeg memory l, uint256 next) = plan.liqLeg(o);
            o = next;
            // Bit i for leg i: the shift is in the intended order.
            // forge-lint: disable-next-line(incorrect-shift)
            if (_liquidateLeg(debtAsset, l, plan)) filled |= 1 << i;
        }
    }

    /// Apply the plan's governance change, if it carries one, and return what
    /// it cost this transaction: its gas times `tx.gasprice` when it ran, zero
    /// when it was skipped (already applied, not due, or not a genuine plan).
    ///  - Aave payload: `executePayload(id)` on the fixed PayloadsController.
    ///  - Sky spell: `cast()` only when DSPause holds the plan the spell's own
    ///    fields hash to, i.e. governance scheduled it. The fields are read
    ///    with staticcalls; a contract that only imitates a spell cannot pass.
    function govExec(uint8 flags, uint40 payloadId, address spell)
        external payable onlyDelegated returns (uint256 cost)
    {
        uint256 start = gasleft();
        bool ran;
        if (flags & PlanDecoder.FLAG_GOV_EXEC != 0) {
            try IPayloadsController(MainnetVenues.AAVE_PAYLOADS_CONTROLLER).executePayload(payloadId) {
                ran = true;
            } catch (bytes memory r) {
                emit GovExecSkipped(payloadId, _clip(r));
            }
        } else if (flags & PlanDecoder.FLAG_GOV_SPELL != 0) {
            if (_spellPlotted(spell)) {
                try IDssSpell(spell).cast() {
                    ran = true;
                } catch (bytes memory r) {
                    emit GovSpellSkipped(spell, _clip(r));
                }
            } else {
                emit GovSpellSkipped(spell, "");
            }
        }
        if (ran) cost = (start - gasleft()) * tx.gasprice;
    }

    function _spellPlotted(address spell) internal view returns (bool) {
        try IDssSpell(spell).action() returns (address usr) {
            bytes32 tag = IDssSpell(spell).tag();
            bytes memory fax = IDssSpell(spell).sig();
            uint256 eta = IDssSpell(spell).eta();
            return IDSPause(MainnetVenues.SKY_PAUSE).plans(keccak256(abi.encode(usr, tag, fax, eta)));
        } catch {
            return false;
        }
    }

    /// Cap revert data so a protocol returning a huge blob cannot make the
    /// log dominate the gas cost of the leg it describes.
    function _clip(bytes memory r) internal pure returns (bytes memory) {
        if (r.length <= 256) return r;
        bytes memory out = new bytes(256);
        for (uint256 i; i < 256; ++i) out[i] = r[i];
        return out;
    }

    // ──────────────────────────────────────────────────────────────────────
    // On-chain adapters. Each: exact approval → try/catch the real
    // liquidation call → clear the allowance. Returns false — rather than
    // reverting — when the position is gone or the protocol rejects, so the
    // rest of the batch survives.
    //
    // No health pre-check. Every protocol here computes the position's
    // health inside its liquidation call and reverts (Compound: returns an
    // error code) when it is not liquidatable, which the try/catch turns
    // into a skipped leg. Asking its health view first repeated that work on
    // every leg that filled: 26k–30k gas on Aave V3, 36k on Aave V4, 39k on
    // Compound, 63k on Euler, 90k on Silo (mainnet fork, whole transaction
    // after refunds, with and without the view). A leg that does not fill
    // now costs its approval and the reverted call instead of the view.
    //
    // No `seized` return anywhere. With several collaterals in flight, sizing
    // swaps from per-leg deltas means a map; the swap blob takes whole balances
    // instead (`L_TAKE_BALANCE`), which cannot disagree with what is held —
    // that is also how "read back actual repaid/seized" (GUIDE 10 §4) is met:
    // nothing downstream trusts the requested amount.
    //
    // The allowance is cleared after EVERY call, success or failure: V3 clamps
    // `debtToCover` to its close factor and V4 to its target-HF maximum, so a
    // successful pull can consume less than approved. `safeApprove(…, 0)`
    // reads the allowance and writes only when something is left. GUIDE 10 §5.
    //
    // `market` comes from the plan, so it is only ever a registry address the
    // operator encoded. try/catch does not bound gas, and a market that burns
    // the call's gas would take the batch down with it — acceptable only
    // because that set is curated. Do not widen it to arbitrary input.
    // ──────────────────────────────────────────────────────────────────────
    function _liquidateLeg(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool)
    {
        if (l.adapter == PlanDecoder.A_AAVE_V3)  return _liquidateAaveV3(debtAsset, l);
        if (l.adapter == PlanDecoder.A_AAVE_V4)  return _liquidateAaveV4(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_MORPHO)   return _liquidateMorpho(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_EULER)    return _liquidateEulerV2(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_SILO)     return _liquidateSiloV2(debtAsset, l);
        if (l.adapter == PlanDecoder.A_LIQUITY)  return _liquidateLiquityV2(l, plan);
        if (l.adapter == PlanDecoder.A_FLUID)    return _liquidateFluid(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_GEARBOX)  return _liquidateGearbox(debtAsset, l, plan);
        if (l.adapter == PlanDecoder.A_COMPOUND) return _liquidateCompoundV2(debtAsset, l, plan);
        revert UnknownAdapter(l.adapter); // unreachable: the decoder rejected it
    }

    /// Aave V3 `Pool.liquidationCall(collateral, debt, user, debtToCover, false)`
    /// pin 8305565ae. The call is the guard: it computes the user's health
    /// factor and reverts unless it is below 1 (`validateLiquidationCall`).
    function _liquidateAaveV3(address debtAsset, LiqLeg memory l) internal returns (bool ok) {
        debtAsset.safeApprove(l.market, l.repayAmount);
        try IAavePool(l.market).liquidationCall(
            l.collateralAsset, debtAsset, l.borrower, l.repayAmount,
            false   // never receive aTokens: profit must converge on WETH
        ) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_AAVE_V3, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Aave V4 `Spoke.liquidationCall(collateralReserveId, debtReserveId, user,
    /// debtToCover, false)` pin 40232a0a. Reserve ids come from the leg tail;
    /// the encoder (`liq-plan::validate`) pins them to the leg's addresses via
    /// the adapter config — the contract has no address→id view to check
    /// against and the profit guard bounds any mismatch. The call is the
    /// guard: the Spoke reverts unless the user's health factor is below 1.
    /// The protocol clamps `debtToCover` to its target-HF maximum; the clear
    /// below and the balance-based swaps are what make that safe.
    function _liquidateAaveV4(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (uint16 collId, uint16 debtId) = plan.tailV4(l.tailOffset);
        debtAsset.safeApprove(l.market, l.repayAmount);
        try IAaveV4Spoke(l.market).liquidationCall(
            collId, debtId, l.borrower, l.repayAmount,
            false   // never receive shares
        ) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_AAVE_V4, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Morpho Blue `liquidate(marketParams, borrower, 0, repaidShares, "")`
    /// pin 8e26ca6a. Morpho has no health view; its own `_isHealthy` inside
    /// `liquidate` is the guard (`HEALTHY_POSITION` → caught → leg skipped).
    ///
    /// `repayAmount` is loan assets; Morpho takes shares. Convert with the
    /// post-accrual totals Morpho itself will use — `accrueInterest` is
    /// idempotent within a block, so the totals read here are exactly the ones
    /// `liquidate` sees. `toSharesDown(a)` then `toAssetsUp(shares) <= a`, so
    /// the exact approval always covers the pull. Cap at the borrower's shares:
    /// a full-close quote is `toAssetsUp(borrowShares)`, and converting that
    /// back down can land one share above what is owed — which would revert
    /// inside Morpho and skip a liquidatable position.
    function _liquidateMorpho(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        bytes32 id = plan.tailMorpho(l.tailOffset);
        IMorpho morpho = IMorpho(l.market);

        MarketParams memory mp = morpho.idToMarketParams(id);
        if (mp.loanToken != debtAsset || mp.collateralToken != l.collateralAsset) revert LegMismatch();

        IMorpho.Market memory m;
        IMorpho.Position memory pos;
        try morpho.accrueInterest(mp) {
            try morpho.market(id) returns (IMorpho.Market memory m_) {
                m = m_;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_GUARD, _clip(r));
                return false;
            }
            try morpho.position(id, l.borrower) returns (IMorpho.Position memory p_) {
                pos = p_;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_GUARD, _clip(r));
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        if (pos.borrowShares == 0) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_NOT_LIQ, "");
            return false;
        }

        // SharesMathLib.toSharesDown: same expression, same operands as Morpho.
        uint256 shares = (uint256(l.repayAmount) * (uint256(m.totalBorrowShares) + MORPHO_VIRTUAL_SHARES))
            / (uint256(m.totalBorrowAssets) + MORPHO_VIRTUAL_ASSETS);
        if (shares > pos.borrowShares) shares = pos.borrowShares;
        if (shares == 0) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_SIZING, "");
            return false;
        }

        debtAsset.safeApprove(l.market, l.repayAmount);
        try morpho.liquidate(mp, l.borrower, 0, shares, "") returns (uint256, uint256) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_MORPHO, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Euler V2 `IEVault.liquidate(violator, collateral, repayAssets, minYieldBalance)`
    /// pin `bfb325a6`. Target = debt vault (`market`). Tail vault is the
    /// collateral vault (shares). `collateralAsset` is the underlying the
    /// swaps sell. The ABI has no receive-underlying flag, so the seized
    /// shares are redeemed before the swap.
    ///
    /// The call is the guard (deployed liquidation module
    /// 0x16fa62D8c322a6156fb5eF267342A3C7952AD23C, Sourcify): a healthy
    /// violator has a maximum repay of zero and `liquidate` reverts
    /// `E_ExcessiveRepayAmount`; a yield under the tail's minimum reverts
    /// `E_MinYield`. A violator with no debt left is a no-op there unless
    /// that minimum is above zero, so a zero minimum is refused here: the
    /// leg would count as filled having seized nothing.
    function _liquidateEulerV2(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (uint256 minYield, address vault) = plan.tailEuler(l.tailOffset);
        if (vault == address(0) || minYield == 0) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_TAIL, "");
            return false;
        }

        address connector;
        try IEVault(l.market).EVC() returns (address e) {
            connector = e;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        if (connector == address(0)) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_GUARD, "");
            return false;
        }

        // `liquidate` transfers the violator's debt onto the caller and seizes
        // shares (`transferBorrow`). `enableCollateral` puts the seized vault
        // into the account's collateral set BEFORE `liquidate` runs, so the
        // controller's deferred account-status check (fired by
        // `enableController` and resolved at the end of the batch) sees it.
        // The caller's account check reverts `E_AccountLiquidity` unless the
        // assumed debt is repaid first — `repay(uint256.max)` pulls the
        // underlying and clears it. The controller is released in the same
        // batch so a later debt vault can be enabled, and `disableController`
        // itself reverts `E_OutstandingDebt` if the repay left anything open,
        // so the transaction cannot end holding Euler debt. `redeem` last:
        // it only needs the shares this contract already holds, not
        // controller/collateral state, and running it after `disableController`
        // keeps the batch in the sequence the mechanism was verified against.
        IEVC.BatchItem[] memory items = new IEVC.BatchItem[](6);
        items[0] = IEVC.BatchItem({
            targetContract: connector,
            onBehalfOfAccount: address(0),
            value: 0,
            data: abi.encodeCall(IEVC.enableController, (address(this), l.market))
        });
        items[1] = IEVC.BatchItem({
            targetContract: connector,
            onBehalfOfAccount: address(0),
            value: 0,
            data: abi.encodeCall(IEVC.enableCollateral, (address(this), vault))
        });
        items[2] = IEVC.BatchItem({
            targetContract: l.market,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.liquidate, (l.borrower, vault, l.repayAmount, minYield))
        });
        items[3] = IEVC.BatchItem({
            targetContract: l.market,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.repay, (type(uint256).max, address(this)))
        });
        items[4] = IEVC.BatchItem({
            targetContract: l.market,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.disableController, ())
        });
        items[5] = IEVC.BatchItem({
            targetContract: vault,
            onBehalfOfAccount: address(this),
            value: 0,
            data: abi.encodeCall(IEVault.redeem, (type(uint256).max, address(this), address(this)))
        });

        uint256 sharesBefore = IERC20(vault).balanceOf(address(this));
        debtAsset.safeApprove(l.market, l.repayAmount);
        try IEVC(connector).batch(items) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_EULER, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
        if (ok) {
            uint256 sharesNow = IERC20(vault).balanceOf(address(this));
            if (sharesNow > sharesBefore) {
                IEVault(vault).redeem(sharesNow - sharesBefore, address(this), address(this));
            }
        }
    }

    /// Silo V2 `IPartialLiquidation.liquidationCall` pin `570a668a` topic0
    /// `0x3a84f644…`. Target = hook receiver. The call is the guard: the
    /// hook reverts for a solvent borrower.
    ///
    /// S3. `_receiveSToken` is always `false`. The hook then redeems the
    /// seized shares to the caller itself, and a collateral silo short of
    /// liquidity reverts that redeem (`NotEnoughLiquidity`) and the call
    /// with it: the leg is skipped (deployed `SiloHookV1`
    /// 0xc51f048279705a9427983DCB2813c06af1dA3f5b and `Silo`
    /// 0xef1bc66e0ea9717a3f2c969633a989d6bf41024b, Blockscout). Passing
    /// `true` would "succeed" and leave the Executor holding Silo shares it
    /// has no redeem path for, with a swap built to sell the underlying it
    /// does not have — stuck funds, not a skipped opportunity. `false`
    /// fails closed until an sToken redeem path exists.
    function _liquidateSiloV2(address debtAsset, LiqLeg memory l) internal returns (bool ok) {
        debtAsset.safeApprove(l.market, l.repayAmount);
        try ISiloHook(l.market).liquidationCall(
            l.collateralAsset, debtAsset, l.borrower, l.repayAmount, false
        ) returns (uint256, uint256) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_SILO, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        debtAsset.safeApprove(l.market, 0);
    }

    /// Liquity V2 `batchLiquidateTroves(uint256[])` selector `0xef49a6b4`
    /// pin `c8a5a4ee`. No token repay (Stability Pool is the counterparty).
    /// Guard: `getTroveStatus` ∈ {active=1, zombie=4}. `NothingToLiquidate`
    /// / `EmptyData` → catch → skip. Gas compensation is a WETH
    /// `transferFrom` from the gas pool plus `sendColl` of the branch
    /// collateral (pin `c8a5a4ee`). A native-ETH delta, if one arrives, is
    /// wrapped; the WETH transfer is already in the WETH balance `gross` reads.
    function _liquidateLiquityV2(LiqLeg memory l, bytes calldata plan) internal returns (bool ok) {
        uint256 id = plan.tailU256(l.tailOffset);
        if (id == 0) {
            emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_TAIL, "");
            return false;
        }
        try ITroveManager(l.market).getTroveStatus(id) returns (uint8 status) {
            if (status != 1 && status != 4) {
                emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_NOT_LIQ, "");
                return false;
            }
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }

        uint256[] memory ids = new uint256[](1);
        ids[0] = id;
        uint256 ethBefore = address(this).balance;
        try ITroveManager(l.market).batchLiquidateTroves(ids) {
            ok = true;
            uint256 ethNow = address(this).balance;
            if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_LIQUITY, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
    }

    /// Fluid vault liquidation, pin `9496626f`. `market` = the vault;
    /// `repayAmount` = the debt token we pay, exactly (WETH when the vault's
    /// debt is native ETH); `collateralAsset` = the one collateral token we
    /// take (WETH when native). The tail names the vault type — four ABIs —
    /// and the one-token choice on each smart side. No HF view: the call is
    /// the guard. Its floors: `colPerUnitDebt` (collateral per unit of debt,
    /// the vault's own check), and on smart sides `debtSharesMin` (shares the
    /// repay must burn) and the per-share withdraw minimum.
    /// Native ETH: WETH is unwrapped for `msg.value`; every wei that comes
    /// back — the vault's refund and native collateral — is wrapped again.
    function _liquidateFluid(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        FluidTail memory t = plan.tailFluid(l.tailOffset);
        if (t.vaultType < 1 || t.vaultType > 4) {
            emit LegFailed(PlanDecoder.A_FLUID, l.market, l.borrower, ST_SIZING, "");
            return false;
        }
        uint256 pay = l.repayAmount;
        uint256 ethBefore = address(this).balance;
        uint256 value;
        if (t.flags & PlanDecoder.FLUID_NATIVE_DEBT != 0) {
            try IWETH(WETH).withdraw(pay) {} catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_FLUID, l.market, l.borrower, ST_SIZING, _clip(r));
                return false;
            }
            value = pay;
        } else {
            debtAsset.safeApprove(l.market, pay);
        }

        ok = _callFluid(t, l, pay, value);

        if (value == 0) debtAsset.safeApprove(l.market, 0);
        uint256 ethNow = address(this).balance;
        if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
    }

    /// One typed call per vault type (typed so a codeless `market` reverts
    /// instead of "succeeding"). A smart side uses one token: the other
    /// token's amount or per-share figure is zero.
    function _callFluid(FluidTail memory t, LiqLeg memory l, uint256 pay, uint256 value)
        internal returns (bool)
    {
        bool absorb = t.flags & PlanDecoder.FLUID_ABSORB != 0;
        bool debt1 = t.flags & PlanDecoder.FLUID_DEBT_TOKEN1 != 0;
        bool col1 = t.flags & PlanDecoder.FLUID_COL_TOKEN1 != 0;
        uint256 sharesMin = (pay * t.debtSharesMinPerToken) / 1e18;
        bytes memory r;
        if (t.vaultType == 1) {
            try IFluidT1(l.market).liquidate{value: value}(pay, t.colPerUnitDebt, address(this), absorb)
                returns (uint256, uint256)
            {
                return true;
            } catch (bytes memory e) {
                r = e;
            }
        } else if (t.vaultType == 2) {
            try IFluidT2(l.market).liquidate{value: value}(
                pay,
                t.colPerUnitDebt,
                col1 ? 0 : t.colPerShareMin,
                col1 ? t.colPerShareMin : 0,
                address(this),
                absorb
            ) returns (uint256, uint256, uint256, uint256) {
                return true;
            } catch (bytes memory e) {
                r = e;
            }
        } else if (t.vaultType == 3) {
            try IFluidT3(l.market).liquidate{value: value}(
                debt1 ? 0 : pay,
                debt1 ? pay : 0,
                sharesMin,
                t.colPerUnitDebt,
                address(this),
                absorb
            ) returns (uint256, uint256) {
                return true;
            } catch (bytes memory e) {
                r = e;
            }
        } else {
            try IFluidT4(l.market).liquidate{value: value}(
                debt1 ? 0 : pay,
                debt1 ? pay : 0,
                sharesMin,
                t.colPerUnitDebt,
                col1 ? 0 : t.colPerShareMin,
                col1 ? t.colPerShareMin : 0,
                address(this),
                absorb
            ) returns (uint256, uint256, uint256, uint256) {
                return true;
            } catch (bytes memory e) {
                r = e;
            }
        }
        emit LegFailed(PlanDecoder.A_FLUID, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        return false;
    }

    /// Gearbox V3 (`market` = facade, `borrower` = credit account,
    /// `debtAsset` = the manager's underlying). Tail = minimum collateral
    /// received ‖ mode. **Approvals go to the credit manager**, which does
    /// every pull as spender; an allowance held by the facade is never used.
    ///
    /// Mode 0 — `partiallyLiquidateCreditAccount` (v3.1): repay
    /// `repayAmount`, seize `collateralAsset` at the discount; the facade
    /// enforces `minSeized` and requires the account to end healthy.
    ///
    /// Mode 1 — full `liquidateCreditAccount(account, this, calls)` (the
    /// 3-arg form: v3.0, and v3.1's wrapper with empty loss-policy data).
    /// Multicall: `addCollateral(underlying, repayAmount)` then
    /// `withdrawCollateral(collateralAsset, max, this)`. The manager pays the
    /// pool from the account's underlying, keeps the borrower's share on the
    /// account and returns the rest of the underlying to us, so over-adding
    /// comes back. Too little reverts in Gearbox → the leg fails. The facade
    /// has no slip check on withdrawn collateral, so `minSeized` is enforced
    /// here after the call — short is a whole-plan revert.
    function _liquidateGearbox(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (uint256 minSeized, uint8 mode) = plan.tailGearbox(l.tailOffset);
        if (mode > 1) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_TAIL, "");
            return false;
        }

        address puller;
        try ICreditFacadeV3(l.market).creditManager() returns (address m) {
            puller = m;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_GUARD, _clip(r));
            return false;
        }
        if (puller == address(0)) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_GUARD, "");
            return false;
        }

        debtAsset.safeApprove(puller, l.repayAmount);
        if (mode == 0) {
            PriceUpdate[] memory none;
            try ICreditFacadeV3(l.market).partiallyLiquidateCreditAccount(
                l.borrower, l.collateralAsset, l.repayAmount, minSeized, address(this), none
            ) returns (uint256) {
                ok = true;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
            }
        } else {
            ok = _liquidateGearboxFull(debtAsset, l, minSeized);
        }
        debtAsset.safeApprove(puller, 0);
    }

    function _liquidateGearboxFull(address debtAsset, LiqLeg memory l, uint256 minSeized)
        internal returns (bool ok)
    {
        MultiCall[] memory calls = new MultiCall[](2);
        calls[0] = MultiCall({
            target: l.market,
            callData: abi.encodeCall(ICreditFacadeV3Multicall.addCollateral, (debtAsset, l.repayAmount))
        });
        calls[1] = MultiCall({
            target: l.market,
            callData: abi.encodeCall(
                ICreditFacadeV3Multicall.withdrawCollateral, (l.collateralAsset, type(uint256).max, address(this))
            )
        });
        uint256 collBefore = IERC20(l.collateralAsset).balanceOf(address(this));
        try ICreditFacadeV3(l.market).liquidateCreditAccount(l.borrower, address(this), calls) {
            ok = true;
        } catch (bytes memory r) {
            emit LegFailed(PlanDecoder.A_GEARBOX, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
        }
        if (ok) {
            uint256 got = IERC20(l.collateralAsset).balanceOf(address(this)) - collBefore;
            if (got < minSeized) revert SeizedBelowMin(got, minSeized);
        }
    }

    /// Compound V2 official Unitroller pin `a3214f67`. `market` = debt
    /// cToken. Tail = cTokenCollateral ‖ isCEther. The call is the guard:
    /// `liquidateBorrow` asks the Comptroller's `liquidateBorrowAllowed`
    /// (a shortfall, or a deprecated market) and fails without it — a
    /// CErc20 by return code, CEther by revert. Never receive
    /// cTokens as a flag — seize lands as cTokens. This leg redeems that
    /// delta to underlying (CEther: ETH, then wrapped) before the swaps.
    /// CEther debt: unwrap WETH, official 2-arg payable
    /// `liquidateBorrow`, wrap only ETH gained by this leg. Wrong
    /// `isCEther` / repay > WETH / withdraw-or-liq revert skips the **leg**.
    function _liquidateCompoundV2(address debtAsset, LiqLeg memory l, bytes calldata plan)
        internal returns (bool ok)
    {
        (address cTokenColl, uint8 isCEther) = plan.tailCompound(l.tailOffset);
        if (isCEther > 1) {
            emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_TAIL, "");
            return false;
        }

        uint256 seizedBefore = IERC20(cTokenColl).balanceOf(address(this));
        if (isCEther != 0) {
            if (debtAsset != WETH) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_TAIL, "");
                return false;
            }
            uint256 need = l.repayAmount;
            if (IERC20(WETH).balanceOf(address(this)) < need) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_SIZING, "");
                return false;
            }
            uint256 ethBefore = address(this).balance;
            try IWETH(WETH).withdraw(need) {} catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_SIZING, _clip(r));
                return false;
            }
            try ICEther(l.market).liquidateBorrow{value: need}(l.borrower, cTokenColl) {
                ok = true;
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
            }
            uint256 ethNow = address(this).balance;
            if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
        } else {
            debtAsset.safeApprove(l.market, l.repayAmount);
            try ICErc20(l.market).liquidateBorrow(l.borrower, l.repayAmount, cTokenColl)
                returns (uint256 errCode)
            {
                ok = errCode == 0;
                if (!ok) {
                    // Compound signals failure by return code, not revert.
                    emit LegFailed(
                        PlanDecoder.A_COMPOUND, l.market, l.borrower,
                        ST_LIQUIDATE, abi.encode(errCode)
                    );
                }
            } catch (bytes memory r) {
                emit LegFailed(PlanDecoder.A_COMPOUND, l.market, l.borrower, ST_LIQUIDATE, _clip(r));
            }
            debtAsset.safeApprove(l.market, 0);
        }
        if (ok) _redeemSeizedCToken(cTokenColl, seizedBefore);
    }

    /// Redeem only the cTokens this leg seized. CEther pays ETH; wrap that
    /// delta so `gross` sees WETH. A non-zero Compound error code reverts
    /// the transaction: the liquidation must not stand with cTokens stuck.
    function _redeemSeizedCToken(address cToken, uint256 seizedBefore) internal {
        uint256 seizedNow = IERC20(cToken).balanceOf(address(this));
        if (seizedNow <= seizedBefore) return;
        uint256 ethBefore = address(this).balance;
        // cETH at older implementations returns no data on success. A newer
        // cToken returns the Compound error code. A non-zero code reverts;
        // empty return data does not.
        (bool redeemed, bytes memory ret) =
            cToken.call(abi.encodeWithSelector(ICToken.redeem.selector, seizedNow - seizedBefore));
        if (!redeemed) {
            uint256 code;
            if (ret.length >= 32) code = abi.decode(ret, (uint256));
            revert RedeemFailed(cToken, code);
        }
        if (ret.length >= 32) {
            uint256 code = abi.decode(ret, (uint256));
            if (code != 0) revert RedeemFailed(cToken, code);
        }
        uint256 ethNow = address(this).balance;
        if (ethNow > ethBefore) IWETH(WETH).deposit{value: ethNow - ethBefore}();
    }
}
