//! Five `FlashSource` implementors. Adding a source is one file here plus a
//! `mod` line.

#![allow(clippy::too_many_arguments)] // sol! event ctors mirror ABI arity

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::DecodedLog;
use liq_types::AssetId;

pub mod aave;
pub mod morpho;
pub mod sky_dss;
pub mod univ3;
pub mod univ4;

pub use aave::{AavePool, AaveReserve};
pub use morpho::MorphoBlue;
pub use sky_dss::SkyDssFlash;
pub use univ3::UniV3Pool;
pub use univ4::UniV4PoolManager;

sol! {
    interface IERC20 {
        event Transfer(address indexed from, address indexed to, uint256 value);
    }
}

/// One interned ERC-20 held at a single `holder` (V4 PoolManager, Morpho).
#[derive(Clone, Debug)]
pub struct HeldAsset {
    pub asset: AssetId,
    pub token: Address,
    pub balance: U256,
}

#[derive(Clone, Debug)]
struct TokenSlot {
    token: Address,
    holder: Address,
    balance: U256,
}

struct SlotTable {
    slots: Vec<Option<TokenSlot>>,
}

impl SlotTable {
    fn from_held(holder: Address, assets: &[HeldAsset]) -> Self {
        let mut slots = Vec::new();
        for a in assets {
            if a.token.is_zero() {
                continue;
            }
            let i = usize::from(a.asset.0);
            if slots.len() <= i {
                slots.resize(i.saturating_add(1), None);
            }
            if let Some(slot) = slots.get_mut(i) {
                *slot = Some(TokenSlot {
                    token: a.token,
                    holder,
                    balance: a.balance,
                });
            }
        }
        Self { slots }
    }

    #[inline]
    fn available(&self, asset: AssetId) -> U256 {
        match self.slots.get(usize::from(asset.0)) {
            Some(Some(s)) => s.balance,
            _ => U256::ZERO,
        }
    }

    fn apply_transfer(&mut self, log: &DecodedLog<'_>) {
        for slot in self.slots.iter_mut().flatten() {
            apply_holder_transfer(slot.token, slot.holder, &mut slot.balance, log);
        }
    }

    fn tokens(&self) -> impl Iterator<Item = Address> + '_ {
        self.slots.iter().flatten().map(|s| s.token)
    }
}

/// ERC-20 `Transfer` touching `holder` of `token`. Fail-closed: overflow or
/// underflow zeros the tracked balance (cannot fund), never wraps.
fn apply_holder_transfer(
    token: Address,
    holder: Address,
    balance: &mut U256,
    log: &DecodedLog<'_>,
) {
    if log.address != token {
        return;
    }
    let Some((credit, amt)) = transfer_delta(holder, log) else {
        return;
    };
    *balance = if credit {
        balance.checked_add(amt).unwrap_or(U256::ZERO)
    } else {
        balance.checked_sub(amt).unwrap_or(U256::ZERO)
    };
}

fn transfer_delta(holder: Address, log: &DecodedLog<'_>) -> Option<(bool, U256)> {
    let ev = IERC20::Transfer::decode_raw_log(log.topics.iter().copied(), log.data).ok()?;
    if ev.from == holder && ev.to == holder {
        return None;
    }
    if ev.to == holder {
        return Some((true, ev.value));
    }
    if ev.from == holder {
        return Some((false, ev.value));
    }
    None
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used
)]
pub(crate) mod fixtures {
    use alloy_primitives::{address, b256, Address, B256, U256};
    use liq_protocol::DecodedLog;
    use liq_types::AssetId;

    /// Oracle: chain, block 26_000_000 (`eth_call` via eth.drpc.org).
    pub(crate) const BLOCK_26M: u64 = 26_000_000;

    pub(crate) const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    pub(crate) const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    pub(crate) const DAI: Address = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");

    pub(crate) const AAVE_POOL: Address = address!("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
    pub(crate) const AAVE_CONFIGURATOR: Address =
        address!("0x64b761D848206f447Fe2dd461b0c635Ec39EbB27");
    pub(crate) const A_USDC: Address = address!("0x98C23E9d8f34FEFb1B7BD6a91B7FF122F4e16F5c");
    pub(crate) const A_WETH: Address = address!("0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8");

    pub(crate) const UNIV3_USDC_WETH_500: Address =
        address!("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
    pub(crate) const UNIV3_USDC_WETH_100: Address =
        address!("0xE0554a476A092703abdB3Ef35c80e0D76d32939F");
    pub(crate) const POOL_MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
    pub(crate) const MORPHO: Address = address!("0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
    /// Chainlog `MCD_FLASH` at 26M.
    pub(crate) const DSS_FLASH: Address = address!("0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA");
    /// Chainlog `MCD_VAT`. Emits no events; used only as a negative fixture.
    pub(crate) const VAT: Address = address!("0x35D1b3F3D7966A1DFe207aa4514C12a259A0492B");
    /// Chainlog `MCD_END`. Emits `Cage()`; `Vat.wards(End) == 1`.
    pub(crate) const END: Address = address!("0x0e2e8F1D1326A4B9633D96222Ce399c708B19c28");

    /// `IERC20(USDC).balanceOf(aUSDC)` at block 26_000_000.
    pub(crate) const AUSDC_USDC_26M: u128 = 181_646_545_035_048;
    /// `IERC20(WETH).balanceOf(aWETH)` at block 26_000_000.
    pub(crate) const AWETH_WETH_26M: &str = "288592198471713941942983";
    /// UniV3 5-bp USDC/WETH pool balances at 26_000_000.
    pub(crate) const V3_500_USDC_26M: u128 = 74_293_828_839_265;
    pub(crate) const V3_500_WETH_26M: &str = "12163941530336152750397";
    /// PoolManager ERC-20 balances at 26_000_000.
    pub(crate) const PM_USDC_26M: u128 = 66_230_362_793_739;
    pub(crate) const PM_WETH_26M: &str = "2131150728309835187612";
    /// Morpho singleton balances at 26_000_000.
    pub(crate) const MORPHO_USDC_26M: u128 = 108_682_339_582_079;
    pub(crate) const MORPHO_WETH_26M: &str = "15880325145738137013578";
    /// `DssFlash.max()` at 26_000_000: 500 million DAI (wad).
    pub(crate) const DSS_MAX_26M: &str = "500000000000000000000000000";

    pub(crate) const ID_USDC: AssetId = AssetId(0);
    pub(crate) const ID_WETH: AssetId = AssetId(1);
    pub(crate) const ID_DAI: AssetId = AssetId(2);
    pub(crate) const ID_NATIVE: AssetId = AssetId(3);

    /// Independent keccak (pycryptodome) of the canonical ABI signatures.
    pub(crate) const T0_TRANSFER: B256 =
        b256!("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
    pub(crate) const T0_AAVE_SUPPLY: B256 =
        b256!("0x2b627736bca15cd5381dcf80b0bf11fd197d01a037c52b927a881a10fb73ba61");
    pub(crate) const T0_AAVE_WITHDRAW: B256 =
        b256!("0x3115d1449a7b732c986cba18244e897a450f61e1bb8d589cd2e69e6c8924f9f7");
    pub(crate) const T0_AAVE_BORROW: B256 =
        b256!("0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0");
    pub(crate) const T0_AAVE_REPAY: B256 =
        b256!("0xa534c8dbe71f871f9f3530e97a74601fea17b426cae02e1c5aee42c96c784051");
    pub(crate) const T0_AAVE_LIQ: B256 =
        b256!("0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286");
    /// 03C `aave-v3.md` cfg.flashloanPremiumTotalUpdated.
    pub(crate) const T0_PREMIUM: B256 =
        b256!("0x71aba182c9d0529b516de7a78bed74d49c207ef7e152f52f7ea5d8730138f643");
    pub(crate) const T0_FLASH_LOANING: B256 =
        b256!("0xc8ff3cc5b0fddaa3e6ebbbd7438f43393e4ea30e88b80ad016c1bc094655034d");
    pub(crate) const T0_RESERVE_ACTIVE: B256 =
        b256!("0xc36c7d11ba01a5869d52aa4a3781939dab851cbc9ee6e7fdcedc7d58898a3f1e");
    pub(crate) const T0_RESERVE_PAUSED: B256 =
        b256!("0xe188d542a5f11925d3a3af33703cdd30a43cb3e8066a3cf68b1b57f61a5a94b5");
    pub(crate) const T0_V3_MINT: B256 =
        b256!("0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde");
    pub(crate) const T0_V3_BURN: B256 =
        b256!("0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c");
    pub(crate) const T0_V3_SWAP: B256 =
        b256!("0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
    pub(crate) const T0_V3_FLASH: B256 =
        b256!("0xbdbdb71d7860376ba52b25a5028beea23581364a40522f6bcfb86bb1f2dca633");
    pub(crate) const T0_V4_MODIFY: B256 =
        b256!("0xf208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec");
    pub(crate) const T0_V4_SWAP: B256 =
        b256!("0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");
    pub(crate) const T0_MORPHO_SUPPLY: B256 =
        b256!("0xedf8870433c83823eb071d3df1caa8d008f12f6440918c20d75a3602cda30fe0");
    pub(crate) const T0_MORPHO_WITHDRAW: B256 =
        b256!("0xa56fc0ad5702ec05ce63666221f796fb62437c32db1aa1aa075fc6484cf58fbf");
    pub(crate) const T0_MORPHO_LIQ: B256 =
        b256!("0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41");
    pub(crate) const T0_MORPHO_FLASH: B256 =
        b256!("0xc76f1b4fe4396ac07a9fa55a415d4ca430e72651d37d3401f3bed7cb13fc4f12");
    pub(crate) const T0_FILE: B256 =
        b256!("0xe986e40cc8c151830d4f61050f4fb2e4add8567caad2d5f5496f9158e91fe4c7");
    pub(crate) const T0_CAGE: B256 =
        b256!("0x2308ed18a14e800c39b86eb6ea43270105955ca385b603b64eca89f98ae8fbda");

    pub(crate) fn u256(s: &str) -> U256 {
        U256::from_str_radix(s, 10).expect("fixture decimal")
    }

    pub(crate) fn word(addr: Address) -> B256 {
        B256::left_padding_from(addr.as_slice())
    }

    pub(crate) fn transfer_topics(from: Address, to: Address) -> [B256; 3] {
        [T0_TRANSFER, word(from), word(to)]
    }

    pub(crate) fn u256_be(v: U256) -> [u8; 32] {
        v.to_be_bytes::<32>()
    }

    pub(crate) fn log<'a>(address: Address, topics: &'a [B256], data: &'a [u8]) -> DecodedLog<'a> {
        DecodedLog {
            address,
            topics,
            data,
            block: BLOCK_26M,
            timestamp: 1,
        }
    }

    /// Test-only RPC/LogSource. Any method panics: sources are log-driven.
    pub(crate) struct PanicLogSource;

    impl PanicLogSource {
        pub(crate) fn poll_block(&mut self) -> ! {
            panic!("oracle: GUIDE-07 / D05 — FlashSource must not RPC");
        }
        pub(crate) fn eth_call(&self, _: Address, _: &[u8]) -> ! {
            panic!("oracle: GUIDE-07 / D05 — FlashSource must not RPC");
        }
        pub(crate) fn eth_get_logs(&self) -> ! {
            panic!("oracle: GUIDE-07 / D05 — FlashSource must not RPC");
        }
    }
}
