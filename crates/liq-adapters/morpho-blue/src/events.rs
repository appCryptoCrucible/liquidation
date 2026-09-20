//! Event ABI from `morpho-org/morpho-blue` `EventsLib.sol` @ `8e26ca6a`.

use alloy_sol_types::sol;

sol! {
    struct MarketParams {
        address loanToken;
        address collateralToken;
        address oracle;
        address irm;
        uint256 lltv;
    }

    event SetOwner(address indexed newOwner);
    event SetFee(bytes32 indexed id, uint256 newFee);
    event SetFeeRecipient(address indexed newFeeRecipient);
    event EnableIrm(address indexed irm);
    event EnableLltv(uint256 lltv);
    event CreateMarket(bytes32 indexed id, MarketParams marketParams);
    event Supply(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets, uint256 shares);
    event Withdraw(bytes32 indexed id, address caller, address indexed onBehalf, address indexed receiver, uint256 assets, uint256 shares);
    event Borrow(bytes32 indexed id, address caller, address indexed onBehalf, address indexed receiver, uint256 assets, uint256 shares);
    event Repay(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets, uint256 shares);
    event SupplyCollateral(bytes32 indexed id, address indexed caller, address indexed onBehalf, uint256 assets);
    event WithdrawCollateral(bytes32 indexed id, address caller, address indexed onBehalf, address indexed receiver, uint256 assets);
    event Liquidate(
        bytes32 indexed id,
        address indexed caller,
        address indexed borrower,
        uint256 repaidAssets,
        uint256 repaidShares,
        uint256 seizedAssets,
        uint256 badDebtAssets,
        uint256 badDebtShares
    );
    event FlashLoan(address indexed caller, address indexed token, uint256 assets);
    event SetAuthorization(address indexed caller, address indexed authorizer, address indexed authorized, bool newIsAuthorized);
    event IncrementNonce(address indexed caller, address indexed authorizer, uint256 usedNonce);
    event AccrueInterest(bytes32 indexed id, uint256 prevBorrowRate, uint256 interest, uint256 feeShares);
}

pub mod halt {
    use super::sol;
    sol! {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
        event Initialized(uint64 version);
    }
}
