//! This tests Uint256 max value deposits to the module (it does NOT test deposits to the ethereum
//! contract which are later relayed by the orchestrator).
//! NOTE: In the process of testing the module, the bridge is desync'd due to false validator claims,
//! therefore adding new tests at the end of this one may fail.
use crate::happy_path::test_erc20_deposit_panic;
use crate::unhalt_bridge::get_nonces;
use crate::utils::{create_default_test_config, get_user_key, start_orchestrators, ValidatorKeys};
use crate::OPERATION_TIMEOUT;
use crate::{get_fee, MINER_ADDRESS};
use clarity::{Address as EthAddress, Uint256};
use deep_space::address::Address as CosmosAddress;
use deep_space::{Coin, Contact, Fee, Msg};
use ethereum_gravity::utils::downcast_uint256;
use gravity_proto::gravity::query_client::QueryClient as GravityQueryClient;
use gravity_proto::gravity::{MsgSendToCosmosClaim, QueryErc20ToDenomRequest};
use num::Bounded;
use std::time::Duration;
use tonic::transport::Channel;
use web30::client::Web3;

// Tests end to end bridge function, then asserts a Uint256 max value deposit of overflowing_erc20 succeeds,
// then asserts that other token deposits are unaffected by this transfer,
// and future deposits of overflowing_erc20 are blocked
pub async fn deposit_overflow_test(
    web30: &Web3,
    contact: &Contact,
    keys: Vec<ValidatorKeys>,
    gravity_address: EthAddress,
    erc20_addresses: Vec<EthAddress>,
    grpc_client: GravityQueryClient<Channel>,
) {
    let mut grpc_client = grpc_client;
    let no_relay_market_config = create_default_test_config();
    start_orchestrators(keys.clone(), gravity_address, false, no_relay_market_config).await;
    ///////////////////// SETUP /////////////////////
    let user_keys = get_user_key();
    let dest = user_keys.cosmos_address;
    let dest2 = get_user_key().cosmos_address;
    let overflowing_erc20 = user_keys.eth_address; // new eth address for the max_value tx
    let check_module_erc20 = erc20_addresses[0]; // unrelated erc20 to check module functions
    let overflowing_denom = grpc_client
        .erc20_to_denom(QueryErc20ToDenomRequest {
            erc20: overflowing_erc20.clone().to_string(),
        })
        .await
        .unwrap()
        .into_inner()
        .denom;
    let check_module_denom = grpc_client
        .erc20_to_denom(QueryErc20ToDenomRequest {
            erc20: check_module_erc20.clone().to_string(),
        })
        .await
        .unwrap()
        .into_inner()
        .denom;
    let mut grpc_client = grpc_client.clone();
    let max_amount = Uint256::max_value(); // 2^256 - 1 (max amount possible to send)
    let normal_amount = Uint256::from(30_000_000u64); // an amount we would expect to easily transfer
    let fee = Fee {
        amount: vec![get_fee()],
        gas_limit: 500_000_000u64,
        granter: None,
        payer: None,
    };

    info!("Test initial transfer to increment nonce and verify bridge function");
    test_erc20_deposit_panic(
        web30,
        contact,
        &mut grpc_client,
        dest,
        gravity_address,
        check_module_erc20,
        normal_amount.clone(),
        Some(OPERATION_TIMEOUT),
        None,
    )
    .await;

    ///////////////////// EXECUTION /////////////////////
    let initial_nonce = get_nonces(&mut grpc_client, &keys, &contact.get_prefix()).await[0];
    let initial_block_height =
        downcast_uint256(web30.eth_get_latest_block().await.unwrap().number).unwrap();
    info!("Initial transfer complete, nonce is {}", initial_nonce);

    // NOTE: the dest user's balance should be 1 * normal_amount of check_module_erc20 token
    let mut expected_cosmos_coins = vec![Coin {
        amount: normal_amount.clone(),
        denom: check_module_denom.clone(),
    }];
    check_cosmos_balances(contact, dest, &expected_cosmos_coins).await;

    // Don't judge me! it's really difficult to get a 2^256-1 ERC20 transfer to happen, so we fake it
    // We would want to deploy a custom ERC20 which allows transfers of any amounts for convenience
    // But this simulates that without testing the solidity portion
    submit_false_claims(
        &keys,
        initial_nonce + 1,
        initial_block_height + 1,
        max_amount.clone(),
        dest,
        *MINER_ADDRESS,
        overflowing_erc20,
        contact,
        &fee,
        Some(OPERATION_TIMEOUT),
    )
    .await;

    // NOTE: the dest user's balance should be 1 * normal_amount of check_module_erc20 token and
    // Uint256 max of false_claims_erc20 token
    expected_cosmos_coins.push(Coin {
        amount: max_amount.clone(),
        denom: overflowing_denom.clone(),
    });
    check_cosmos_balances(contact, dest, &expected_cosmos_coins).await;

    // Note: Now the bridge is broken since the ethereum side's event nonce does not match the
    // Cosmos side's event nonce, we are forced to continue lying to keep the charade going

    // Expect this one to succeed as we're using an unrelated token
    submit_false_claims(
        &keys,
        initial_nonce + 2,
        initial_block_height + 2,
        normal_amount.clone(),
        dest,
        *MINER_ADDRESS,
        check_module_erc20,
        contact,
        &fee,
        Some(OPERATION_TIMEOUT),
    )
    .await;
    // NOTE: the dest user's balance should now be 2 * normal_amount of check_module_erc20 token and
    // Uint256 max of false_claims_erc20 token
    expected_cosmos_coins = vec![
        Coin {
            amount: normal_amount.clone() + normal_amount.clone(),
            denom: check_module_denom.clone(),
        },
        Coin {
            amount: max_amount,
            denom: overflowing_denom,
        },
    ];
    check_cosmos_balances(contact, dest, &expected_cosmos_coins).await;

    // Expect this one to fail, there's no supply left of the false_claims_erc20!
    submit_false_claims(
        &keys,
        initial_nonce + 3,
        initial_block_height + 3,
        normal_amount.clone(),
        dest,
        *MINER_ADDRESS,
        overflowing_erc20,
        contact,
        &fee,
        Some(OPERATION_TIMEOUT),
    )
    .await;
    // NOTE: the dest user's balance should still be 2 * normal_amount of check_module_erc20 token and
    // still be Uint256 max of false_claims_erc20 token
    check_cosmos_balances(contact, dest, &expected_cosmos_coins).await;

    // Expect this one to also fail, there's no supply left of the false_claims_erc20, even though account has changed
    submit_false_claims(
        &keys,
        initial_nonce + 4,
        initial_block_height + 4,
        normal_amount.clone(),
        dest2,
        *MINER_ADDRESS,
        overflowing_erc20,
        contact,
        &fee,
        Some(OPERATION_TIMEOUT),
    )
    .await;
    let dest2_bals = contact.get_balances(dest2).await.unwrap();
    assert!(
        dest2_bals.is_empty(),
        "dest2 should have no coins, but they have {:?}",
        dest2_bals
    );
    info!("Successful send of Uint256 max value to cosmos user, unable to overflow the supply!");
}

// Submits a false send to cosmos for all the validators
#[allow(clippy::too_many_arguments)]
pub async fn submit_false_claims(
    keys: &[ValidatorKeys],
    nonce: u64,
    height: u64,
    amount: Uint256,
    cosmos_receiver: CosmosAddress,
    ethereum_sender: EthAddress,
    erc20_address: EthAddress,
    contact: &Contact,
    fee: &Fee,
    timeout: Option<Duration>,
) {
    info!("Beginnning to submit false claims for ALL validators");
    // let mut futures = vec![];
    for (i, k) in keys.iter().enumerate() {
        //let orch_pubkey = k.orch_key.to_public_key(&contact.get_prefix()).unwrap().to_string();
        let orch_addr = k.orch_key.to_address(&contact.get_prefix()).unwrap();
        let claim = MsgSendToCosmosClaim {
            event_nonce: nonce,
            block_height: height,
            token_contract: erc20_address.to_string(),
            amount: amount.to_string(),
            cosmos_receiver: cosmos_receiver.to_string(),
            ethereum_sender: ethereum_sender.to_string(),
            orchestrator: orch_addr.to_string(),
        };
        info!("Oracle number {} submitting false deposit {:?}", i, claim);
        let msg_url = "/gravity.v1.MsgSendToCosmosClaim";
        let msg = Msg::new(msg_url, claim.clone());
        let res = contact
            .send_message(
                &[msg],
                Some("All your bridge are belong to us".to_string()),
                fee.amount.as_slice(),
                timeout,
                k.orch_key,
            )
            .await;
        info!("Oracle {} false claim response {:?}", i, res);
    }
}

// Checks that cosmos_account has each balance specified in expected_cosmos_coins.
// Note: ignores balances not in expected_cosmos_coins
async fn check_cosmos_balances(
    contact: &Contact,
    cosmos_account: CosmosAddress,
    expected_cosmos_coins: &[Coin],
) {
    let curr_balances = contact.get_balances(cosmos_account).await.unwrap();

    let mut num_found = 0;

    // These loops use loop labels, see the documentation on loop labels here for more information
    // https://doc.rust-lang.org/reference/expressions/loop-expr.html#loop-labels
    'outer: for bal in curr_balances.iter() {
        if num_found == expected_cosmos_coins.len() {
            break 'outer; // done searching entirely
        }
        'inner: for j in 0..expected_cosmos_coins.len() {
            if num_found == expected_cosmos_coins.len() {
                break 'outer; // done searching entirely
            }
            if expected_cosmos_coins[j].denom != bal.denom {
                continue;
            }
            info!("found balance {:?}!", bal);
            assert_eq!(expected_cosmos_coins[j].amount, bal.amount);
            num_found += 1;
            break 'inner; // done searching for this particular balance
        }
    }

    assert_eq!(
        num_found,
        curr_balances.len(),
        "did not find the correct balance for each expected coin! found {} of {}",
        num_found,
        curr_balances.len()
    )
}