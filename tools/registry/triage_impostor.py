"""Identify the two symbol-colliding collateral tokens that sit under admitted markets."""

from __future__ import annotations

import json
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
C1 = json.loads((ROOT / "registry" / "registry.json").read_text())
BLOCK = C1["generated_at_block"]
w3 = Web3(Web3.HTTPProvider("https://gateway.tenderly.co/public/mainnet", request_kwargs={"timeout": 60}))
MORPHO = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
TARGETS = {
    "0x3d4762b4bb4b4c922377fe5b887e900d7fb64cdf": "USDT",
    "0x19ebb35279a16207ec4ba82799cc64715065f7f6": "PRIME",
}
MARKETS = [
    "0xa23d5a03779d1b54b2ea1f9224f9a5567594cf43916257514bda4344ec83466d",
    "0xb74aae3bada73b0bd1087bd110d35afaa390de0451e5745091fbb5d296684e1c",
    "0x41c41d0c9aadbf4751f5ee215ed5a16954a4b34e1b70fca5393d4b08858fa3fa",
    "0x755f954513d31d5f24aaf3d0cdc5e913a28383f8ea8ff85be9ffffa7371fb64d",
]
EIP1967_IMPL = 0xA3F0AD74E5423AEBFD80D3EF4346578335A9A72AEAEE59FF6CB3582B35133D50


def cs(a):
    return Web3.to_checksum_address(a)


def sel(s):
    return Web3.keccak(text=s)[:4]


def call(to, data):
    try:
        return w3.eth.call({"to": cs(to), "data": data}, block_identifier=BLOCK)
    except Exception as e:
        return None


for a, claim in TARGETS.items():
    print(f"\n===== {a}  (claims {claim}) =====")
    code = w3.eth.get_code(cs(a), block_identifier=BLOCK)
    print("code_len", len(code))
    for v, typ in (("name()", "s"), ("symbol()", "s"), ("decimals()", "u"), ("totalSupply()", "u"),
                   ("owner()", "a"), ("asset()", "a"), ("underlying()", "a"), ("implementation()", "a"),
                   ("getOwner()", "a"), ("minter()", "a"), ("VERSION()", "s")):
        r = call(a, sel(v))
        if r is None:
            continue
        if typ == "s":
            try:
                out = decode(["string"], r)[0]
            except Exception:
                out = r.rstrip(b"\x00").decode("utf8", "replace").strip("\x00")
        elif typ == "u":
            out = int.from_bytes(r[:32], "big")
        else:
            out = "0x" + r[12:32].hex() if len(r) >= 32 else None
        print(f"  {v:20} {out}")
    slot = w3.eth.get_storage_at(cs(a), EIP1967_IMPL, block_identifier=BLOCK)
    impl = "0x" + slot[-20:].hex()
    print("  eip1967 impl slot   ", impl if int(impl, 16) else "(empty)")
    # deployment: first tx is not fetchable without an indexer; report creation code hash instead
    print("  codehash            ", Web3.keccak(code).hex())

print("\n===== Morpho market params + state (on-chain, block", BLOCK, ") =====")
for mid in MARKETS:
    i = bytes.fromhex(mid[2:])
    mp = call(MORPHO, sel("idToMarketParams(bytes32)") + encode(["bytes32"], [i]))
    st = call(MORPHO, sel("market(bytes32)") + encode(["bytes32"], [i]))
    loan, coll, oracle, irm, lltv = decode(["address", "address", "address", "address", "uint256"], mp)
    sa, ss, ba, bs, lu, fee = decode(
        ["uint128", "uint128", "uint128", "uint128", "uint128", "uint128"], st
    )
    ot = C1["tokens"].get(coll.lower()) or {}
    px = call(oracle, sel("price()"))
    print(f"\n{mid[:20]}…")
    print(f"  loan={loan} coll={coll} lltv={lltv/1e16:.1f}%")
    print(f"  totalSupplyAssets={sa} totalBorrowAssets={ba} lastUpdate={lu}")
    print(f"  oracle={oracle} price()={int.from_bytes(px[:32],'big') if px else 'REVERT'}")
    print(f"  C1 collateral token record: {ot}")
