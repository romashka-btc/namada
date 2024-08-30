//! Shielded and transparent tokens related functions

use std::collections::{BTreeMap, BTreeSet};

use namada_core::address::Address;
use namada_core::collections::HashSet;
use namada_core::masp::addr_taddr;
use namada_events::extend::UserAccount;
use namada_events::{EmitEvents, EventLevel};
use namada_state::{Error, OptionExt, ResultExt};
use namada_token::event::{TokenEvent, TokenOperation};
#[cfg(any(test, feature = "testing"))]
pub use namada_token::testing;
pub use namada_token::{
    storage_key, utils, Amount, DenominatedAmount, Store, Transfer,
};
use namada_token::{MaspTxId, TransparentTransfersRef};
use namada_tx::action::{Action, MaspAction, Write};
use namada_tx::BatchedTx;
use namada_tx_env::TxEnv;

use crate::{update_masp_note_commitment_tree, Ctx, Result, TxResult};

/// A transparent token transfer that can be used in a transaction.
pub fn transfer(
    ctx: &mut Ctx,
    src: &Address,
    dest: &Address,
    token: &Address,
    amount: Amount,
) -> TxResult {
    // The tx must be authorized by the source and destination addresses
    ctx.insert_verifier(src)?;
    ctx.insert_verifier(dest)?;
    if token.is_internal() {
        // Established address tokens do not have VPs themselves, their
        // validation is handled by the `Multitoken` internal address, but
        // internal token addresses have to verify the transfer
        ctx.insert_verifier(token)?;
    }

    namada_token::transfer(ctx, token, src, dest, amount)?;

    ctx.emit(TokenEvent {
        descriptor: "transfer-from-wasm".into(),
        level: EventLevel::Tx,
        operation: TokenOperation::transfer(
            UserAccount::Internal(src.clone()),
            UserAccount::Internal(dest.clone()),
            token.clone(),
            amount.into(),
            namada_token::read_balance(ctx, token, src)?.into(),
            Some(namada_token::read_balance(ctx, token, dest)?.into()),
        ),
    });

    Ok(())
}

/// Transparent and shielded token transfers that can be used in a transaction.
pub fn multi_transfer(
    ctx: &mut Ctx,
    transfers: Transfer,
    tx_data: &BatchedTx,
) -> Result<()> {
    // Effect the transparent multi transfer(s)
    let debited_accounts =
        if let Some(transparent) = transfers.transparent_part() {
            apply_transparent_transfers(ctx, transparent)
                .wrap_err("Transparent token transfer failed")?
        } else {
            HashSet::new()
        };

    // Apply the shielded transfer if there is a link to one
    if let Some(masp_section_ref) = transfers.shielded_section_hash {
        apply_shielded_transfer(
            ctx,
            masp_section_ref,
            debited_accounts,
            tx_data,
        )
        .wrap_err("Shielded token transfer failed")?;
    }
    Ok(())
}

/// Transfer tokens from `sources` to `targets` and submit a transfer event.
/// Returns an `Err` if any source has insufficient balance or if the transfer
/// to any destination would overflow (This can only happen if the total supply
/// doesn't fit in `token::Amount`). Returns a set of debited accounts.
pub fn apply_transparent_transfers(
    ctx: &mut Ctx,
    transfers: TransparentTransfersRef<'_>,
) -> Result<HashSet<Address>> {
    let sources = transfers.sources();
    let targets = transfers.targets();
    let debited_accounts =
        namada_token::multi_transfer(ctx, &sources, &targets)?;

    let mut evt_sources = BTreeMap::new();
    let mut evt_targets = BTreeMap::new();
    let mut post_balances = BTreeMap::new();

    for ((src, token), amount) in sources {
        // The tx must be authorized by the involved address
        ctx.insert_verifier(&src)?;
        if token.is_internal() {
            // Established address tokens do not have VPs themselves, their
            // validation is handled by the `Multitoken` internal address,
            // but internal token addresses have to verify
            // the transfer
            ctx.insert_verifier(&token)?;
        }
        evt_sources.insert(
            (UserAccount::Internal(src.clone()), token.clone()),
            amount.into(),
        );
        post_balances.insert(
            (UserAccount::Internal(src.clone()), token.clone()),
            namada_token::read_balance(ctx, &token, &src)?.into(),
        );
    }

    for ((target, token), amount) in targets {
        // The tx must be authorized by the involved address
        ctx.insert_verifier(&target)?;
        if token.is_internal() {
            // Established address tokens do not have VPs themselves, their
            // validation is handled by the `Multitoken` internal address,
            // but internal token addresses have to verify
            // the transfer
            ctx.insert_verifier(&token)?;
        }
        evt_targets.insert(
            (UserAccount::Internal(target.clone()), token.clone()),
            amount.into(),
        );
        post_balances.insert(
            (UserAccount::Internal(target.clone()), token.clone()),
            namada_token::read_balance(ctx, &token, &target)?.into(),
        );
    }

    ctx.emit(TokenEvent {
        descriptor: "transfer-from-wasm".into(),
        level: EventLevel::Tx,
        operation: TokenOperation::Transfer {
            sources: evt_sources,
            targets: evt_targets,
            post_balances,
        },
    });

    Ok(debited_accounts)
}

/// Apply a shielded transfer
pub fn apply_shielded_transfer(
    ctx: &mut Ctx,
    masp_section_ref: MaspTxId,
    debited_accounts: HashSet<Address>,
    tx_data: &BatchedTx,
) -> Result<()> {
    let shielded = tx_data
        .tx
        .get_masp_section(&masp_section_ref)
        .cloned()
        .ok_or_err_msg("Unable to find required shielded section in tx data")
        .map_err(|err| {
            ctx.set_commitment_sentinel();
            err
        })?;
    utils::handle_masp_tx(ctx, &shielded)
        .wrap_err("Encountered error while handling MASP transaction")?;
    update_masp_note_commitment_tree(&shielded)
        .wrap_err("Failed to update the MASP commitment tree")?;

    ctx.push_action(Action::Masp(MaspAction::MaspSectionRef(
        masp_section_ref,
    )))?;
    // Extract the debited accounts for the masp part of the transfer and
    // push the relative actions
    let vin_addresses =
        shielded
            .transparent_bundle()
            .map_or_else(Default::default, |bndl| {
                bndl.vin
                    .iter()
                    .map(|vin| vin.address)
                    .collect::<BTreeSet<_>>()
            });
    let masp_authorizers: Vec<_> = debited_accounts
        .into_iter()
        .filter(|account| vin_addresses.contains(&addr_taddr(account.clone())))
        .collect();
    if masp_authorizers.len() != vin_addresses.len() {
        return Err(Error::SimpleMessage(
            "Transfer transaction does not debit all the expected accounts",
        ));
    }

    for authorizer in masp_authorizers {
        ctx.push_action(Action::Masp(MaspAction::MaspAuthorizer(authorizer)))?;
    }

    Ok(())
}
