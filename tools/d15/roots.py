"""Protocol singleton roots — confirm at H1; not hand-typed markets."""

ROOTS = {
    "aave_v3_registry": "0xbaA999AC55EAce41CcAE355c77809e68Bb345170",
    "spark_registry": "0x03cFa0C4622FF84E50E75062683F44c9587e6Cc1",
    "morpho_blue": "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
    "ilk_registry": "0x5a464C28D19848f44199D003BeF5ecc87d090F87",
    "fluid_vault_factory": "0x324c5Dc1fC42c7a4D43d92df1eBA58a54d13Bf2d",
    "fluid_vault_resolver": "0xA5C3E16523eeeDDcC34706b0E6bE88b4c6EA95cC",
    "euler_evault_factory": "0x29a56a1b8214D9Cf7c5561811750D5cBDb45CC8e",
    "gearbox_contracts_register": "0xA50d4E7D8946a7c90652339CDBd262c375d54D99",
    "ajna_erc20_factory": "0x6146DD43C5622bB6D12A5240ab9CF4de14eDC625",
    "ajna_erc721_factory": "0x27461199d3b7381De66a85D685828E967E35AF4c",
    "compound_v3_configurator": "0x316f9708bB98af7dA9c68C1C3b5e79039cD336E3",
    "liquity_v2_collateral_registry": "0xf949982b91c8c61e952b3ba942cbbfaef5386684",
}

# D52: before = 0 is safe unless deploy block is known.
KNOWN_BEFORE = {
    "0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb": 18883124,
    "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2": 16291127,
    "0xc3d688b66703497daa19211eedff47f25384cdc3": 15331586,
    "0xa17581a9e3356d9a858b789d68b4d866e593ae94": 17181117,
    "0x316f9708bb98af7da9c68c1c3b5e79039cd336e3": 15331586,
    "0xbaa999ac55eace41ccae355c77809e68bb345170": 16291124,
    "0x5a464c28d19848f44199d003bef5ecc87d090f87": 12807083,
    "0xf949982b91c8c61e952b3ba942cbbfaef5386684": 22283450,
}

ZERO = "0x0000000000000000000000000000000000000000"
