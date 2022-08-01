//! Helpers for making digital signatures using cryptographic keys from the
//! wallet.

use std::rc::Rc;

use borsh::BorshSerialize;
use namada::proto::Tx;
use namada::types::address::{Address, ImplicitAddress};
use namada::types::key::*;
use namada::types::storage::Epoch;
use namada::types::transaction::{hash_tx, Fee, WrapperTx};
use tendermint_config::net::Address as TendermintAddress;

use super::rpc;
use crate::cli::context::WalletAddress;
use crate::cli::{self, args, Context};
use crate::client::tendermint_rpc_types::TxBroadcastData;
use crate::wallet::Wallet;

/// Find the public key for the given address and try to load the keypair
/// for it from the wallet. Panics if the key cannot be found or loaded.
pub async fn find_keypair(
    wallet: &mut Wallet,
    addr: &Address,
    ledger_address: TendermintAddress,
) -> Rc<common::SecretKey> {
    match addr {
        Address::Established(_) => {
            println!(
                "Looking-up public key of {} from the ledger...",
                addr.encode()
            );
            let public_key = rpc::get_public_key(addr, ledger_address)
                .await
                .unwrap_or_else(|| {
                    eprintln!(
                        "No public key found for the address {}",
                        addr.encode()
                    );
                    cli::safe_exit(1);
                });
            wallet.find_key_by_pk(&public_key).unwrap_or_else(|err| {
                eprintln!(
                    "Unable to load the keypair from the wallet for public \
                     key {}. Failed with: {}",
                    public_key, err
                );
                cli::safe_exit(1)
            })
        }
        Address::Implicit(ImplicitAddress(pkh)) => {
            wallet.find_key_by_pkh(pkh).unwrap_or_else(|err| {
                eprintln!(
                    "Unable to load the keypair from the wallet for the \
                     implicit address {}. Failed with: {}",
                    addr.encode(),
                    err
                );
                cli::safe_exit(1)
            })
        }
        Address::Internal(_) => {
            eprintln!(
                "Internal address {} doesn't have any signing keys.",
                addr
            );
            cli::safe_exit(1)
        }
    }
}

/// Sign a transaction with a given signing key or public key of a given signer.
/// If no explicit signer given, use the `default`. If no `default` is given,
/// panics.
///
/// If this is not a dry run, the tx is put in a wrapper and returned along with
/// hashes needed for monitoring the tx on chain.
///
/// If it is a dry run, it is not put in a wrapper, but returned as is.
pub async fn sign_tx(
    mut ctx: Context,
    tx: Tx,
    args: &args::Tx,
    default: Option<&WalletAddress>,
) -> (Context, TxBroadcastData) {
    let (tx, keypair) = if let Some(signing_key) = &args.signing_key {
        let signing_key = ctx.get_cached(signing_key);
        (tx.sign(&signing_key), signing_key)
    } else if let Some(signer) = args.signer.as_ref().or(default) {
        let signer = ctx.get(signer);
        let signing_key =
            find_keypair(&mut ctx.wallet, &signer, args.ledger_address.clone())
                .await;
        (tx.sign(&signing_key), signing_key)
    } else {
        panic!(
            "All transactions must be signed; please either specify the key \
             or the address from which to look up the signing key."
        );
    };
    let epoch = rpc::query_epoch(args::Query {
        ledger_address: args.ledger_address.clone(),
    })
    .await;
    let broadcast_data = if args.dry_run {
        TxBroadcastData::DryRun(tx)
    } else {
        sign_wrapper(&ctx, args, epoch, tx, &keypair).await
    };
    (ctx, broadcast_data)
}

/// Create a wrapper tx from a normal tx. Get the hash of the
/// wrapper and its payload which is needed for monitoring its
/// progress on chain.
pub async fn sign_wrapper(
    ctx: &Context,
    args: &args::Tx,
    epoch: Epoch,
    tx: Tx,
    keypair: &common::SecretKey,
) -> TxBroadcastData {
    let tx = {
        WrapperTx::new(
            Fee {
                amount: args.fee_amount,
                token: ctx.get(&args.fee_token),
            },
            keypair,
            epoch,
            args.gas_limit.clone(),
            tx,
            // TODO: Actually use the fetched encryption key
            Default::default(),
        )
    };

    // We use this to determine when the wrapper tx makes it on-chain
    let wrapper_hash = hash_tx(&tx.try_to_vec().unwrap()).to_string();
    // We use this to determine when the decrypted inner tx makes it
    // on-chain
    let decrypted_hash = tx.tx_hash.to_string();
    TxBroadcastData::Wrapper {
        tx: tx
            .sign(keypair)
            .expect("Wrapper tx signing keypair should be correct"),
        wrapper_hash,
        decrypted_hash,
    }
}
