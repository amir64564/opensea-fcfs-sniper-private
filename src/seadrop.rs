use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

sol! {
    #[sol(rpc)]
    interface ISeaDrop {
        struct PublicDrop {
            uint80 mintPrice;
            uint48 startTime;
            uint48 endTime;
            uint16 maxTotalMintableByWallet;
            uint16 feeBps;
            bool restrictFeeRecipients;
        }

        function getPublicDrop(address nftContract) external view returns (PublicDrop memory);
        function mintPublicDrop(
            address nftContract,
            address feeRecipient,
            address minterIfNotPayer,
            uint256 quantity
        ) external payable;
    }
}

pub fn encode_mint_public_drop(
    nft: Address,
    fee_recipient: Address,
    minter: Address,
    qty: u64,
) -> Bytes {
    Bytes::from(
        ISeaDrop::mintPublicDropCall {
            nftContract: nft,
            feeRecipient: fee_recipient,
            minterIfNotPayer: minter,
            quantity: U256::from(qty),
        }
        .abi_encode(),
    )
}
