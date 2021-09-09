use anyhow::{bail, Context, Result};
use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use bdk::bitcoin::{Amount, Network, PrivateKey, PublicKey, Transaction};
use bdk::miniscript::DescriptorTrait;
use bdk::wallet::AddressIndex;
use bdk::SignOptions;
use cfd_protocol::{
    build_cfd_transactions, commit_descriptor, compute_signature_point, finalize_spend_transaction,
    lock_descriptor, punish_transaction, spending_tx_sighash, Message, OracleParams, Payout,
    PunishParams, TransactionExt, WalletExt,
};
use rand::{CryptoRng, RngCore, SeedableRng};
use rand_chacha::ChaChaRng;
use secp256k1_zkp::{schnorrsig, SecretKey, SECP256K1};
use std::collections::HashMap;

#[test]
fn run_cfd_protocol() {
    let mut rng = ChaChaRng::seed_from_u64(0);

    let maker_lock_amount = Amount::ONE_BTC;
    let taker_lock_amount = Amount::ONE_BTC;

    let oracle = Oracle::new(&mut rng);
    let (event, announcement) = announce(&mut rng);

    let (maker_sk, maker_pk) = make_keypair(&mut rng);
    let (taker_sk, taker_pk) = make_keypair(&mut rng);

    let payouts = vec![
        Payout::new(Message::Win, Amount::from_btc(2.0).unwrap(), Amount::ZERO),
        Payout::new(Message::Lose, Amount::ZERO, Amount::from_btc(2.0).unwrap()),
    ];

    let refund_timelock = 0;

    let maker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();
    let taker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();

    let maker_address = maker_wallet.get_address(AddressIndex::New).unwrap();
    let taker_address = taker_wallet.get_address(AddressIndex::New).unwrap();

    let lock_amount = maker_lock_amount + taker_lock_amount;
    let (maker_revocation_sk, maker_revocation_pk) = make_keypair(&mut rng);
    let (maker_publish_sk, maker_publish_pk) = make_keypair(&mut rng);

    let (taker_revocation_sk, taker_revocation_pk) = make_keypair(&mut rng);
    let (taker_publish_sk, taker_publish_pk) = make_keypair(&mut rng);

    let maker_params = maker_wallet
        .build_party_params(maker_lock_amount, maker_pk)
        .unwrap();
    let taker_params = taker_wallet
        .build_party_params(taker_lock_amount, taker_pk)
        .unwrap();

    let maker_cfd_txs = build_cfd_transactions(
        (
            maker_params.clone(),
            PunishParams {
                revocation_pk: maker_revocation_pk,
                publish_pk: maker_publish_pk,
            },
        ),
        (
            taker_params.clone(),
            PunishParams {
                revocation_pk: taker_revocation_pk,
                publish_pk: taker_publish_pk,
            },
        ),
        OracleParams {
            pk: oracle.public_key(),
            nonce_pk: event.nonce_pk,
        },
        refund_timelock,
        payouts.clone(),
        maker_sk,
    )
    .unwrap();

    let taker_cfd_txs = build_cfd_transactions(
        (
            maker_params,
            PunishParams {
                revocation_pk: maker_revocation_pk,
                publish_pk: maker_publish_pk,
            },
        ),
        (
            taker_params,
            PunishParams {
                revocation_pk: taker_revocation_pk,
                publish_pk: taker_publish_pk,
            },
        ),
        OracleParams {
            pk: oracle.public_key(),
            nonce_pk: event.nonce_pk,
        },
        refund_timelock,
        payouts,
        taker_sk,
    )
    .unwrap();

    let commit_descriptor = commit_descriptor(
        (maker_pk, maker_revocation_pk, maker_publish_pk),
        (taker_pk, taker_revocation_pk, taker_publish_pk),
    );

    let commit_amount = Amount::from_sat(maker_cfd_txs.commit.0.output[0].value);
    assert_eq!(
        commit_amount.as_sat(),
        taker_cfd_txs.commit.0.output[0].value
    );

    {
        let refund_sighash =
            spending_tx_sighash(&taker_cfd_txs.refund.0, &commit_descriptor, commit_amount);
        SECP256K1
            .verify(&refund_sighash, &maker_cfd_txs.refund.1, &maker_pk.key)
            .expect("valid maker refund sig")
    };

    {
        let refund_sighash =
            spending_tx_sighash(&maker_cfd_txs.refund.0, &commit_descriptor, commit_amount);
        SECP256K1
            .verify(&refund_sighash, &taker_cfd_txs.refund.1, &taker_pk.key)
            .expect("valid taker refund sig")
    };

    // TODO: We should not rely on order
    for (maker_cet, taker_cet) in maker_cfd_txs.cets.iter().zip(taker_cfd_txs.cets.iter()) {
        let cet_sighash = {
            let maker_sighash =
                spending_tx_sighash(&maker_cet.0, &commit_descriptor, commit_amount);
            let taker_sighash =
                spending_tx_sighash(&taker_cet.0, &commit_descriptor, commit_amount);

            assert_eq!(maker_sighash, taker_sighash);
            maker_sighash
        };

        let encryption_point = {
            let maker_encryption_point = compute_signature_point(
                &oracle.public_key(),
                &announcement.nonce_pk(),
                maker_cet.2,
            )
            .unwrap();
            let taker_encryption_point = compute_signature_point(
                &oracle.public_key(),
                &announcement.nonce_pk(),
                taker_cet.2,
            )
            .unwrap();

            assert_eq!(maker_encryption_point, taker_encryption_point);
            maker_encryption_point
        };

        let maker_encsig = maker_cet.1;
        maker_encsig
            .verify(SECP256K1, &cet_sighash, &maker_pk.key, &encryption_point)
            .expect("valid maker cet encsig");

        let taker_encsig = taker_cet.1;
        taker_encsig
            .verify(SECP256K1, &cet_sighash, &taker_pk.key, &encryption_point)
            .expect("valid taker cet encsig");
    }

    let lock_descriptor = lock_descriptor(maker_pk, taker_pk);

    {
        let commit_sighash =
            spending_tx_sighash(&maker_cfd_txs.commit.0, &lock_descriptor, lock_amount);
        let commit_encsig = maker_cfd_txs.commit.1;
        commit_encsig
            .verify(
                SECP256K1,
                &commit_sighash,
                &maker_pk.key,
                &taker_publish_pk.key,
            )
            .expect("valid maker commit encsig");
    };

    {
        let commit_sighash =
            spending_tx_sighash(&taker_cfd_txs.commit.0, &lock_descriptor, lock_amount);
        let commit_encsig = taker_cfd_txs.commit.1;
        commit_encsig
            .verify(
                SECP256K1,
                &commit_sighash,
                &taker_pk.key,
                &maker_publish_pk.key,
            )
            .expect("valid taker commit encsig");
    };

    // sign lock transaction

    let mut signed_lock_tx = maker_cfd_txs.lock;
    maker_wallet
        .sign(
            &mut signed_lock_tx,
            SignOptions {
                trust_witness_utxo: true,
                ..Default::default()
            },
        )
        .unwrap();

    taker_wallet
        .sign(
            &mut signed_lock_tx,
            SignOptions {
                trust_witness_utxo: true,
                ..Default::default()
            },
        )
        .unwrap();

    let signed_lock_tx = signed_lock_tx.extract_tx();

    // verify commit transaction

    let commit_tx = maker_cfd_txs.commit.0;
    let maker_sig = maker_cfd_txs.commit.1.decrypt(&taker_publish_sk).unwrap();
    let taker_sig = taker_cfd_txs.commit.1.decrypt(&maker_publish_sk).unwrap();
    let signed_commit_tx = finalize_spend_transaction(
        commit_tx,
        &lock_descriptor,
        (maker_pk, maker_sig),
        (taker_pk, taker_sig),
    )
    .unwrap();

    check_tx_fee(&[&signed_lock_tx], &signed_commit_tx).expect("correct fees for commit tx");

    lock_descriptor
        .address(Network::Regtest)
        .expect("can derive address from descriptor")
        .script_pubkey()
        .verify(
            0,
            lock_amount.as_sat(),
            bitcoin::consensus::serialize(&signed_commit_tx).as_slice(),
        )
        .expect("valid signed commit transaction");

    // verify refund transaction

    let maker_sig = maker_cfd_txs.refund.1;
    let taker_sig = taker_cfd_txs.refund.1;
    let signed_refund_tx = finalize_spend_transaction(
        maker_cfd_txs.refund.0,
        &commit_descriptor,
        (maker_pk, maker_sig),
        (taker_pk, taker_sig),
    )
    .unwrap();

    check_tx_fee(&[&signed_commit_tx], &signed_refund_tx).expect("correct fees for refund tx");

    commit_descriptor
        .address(Network::Regtest)
        .expect("can derive address from descriptor")
        .script_pubkey()
        .verify(
            0,
            commit_amount.as_sat(),
            bitcoin::consensus::serialize(&signed_refund_tx).as_slice(),
        )
        .expect("valid signed refund transaction");

    // verify cets

    let attestations = [Message::Win, Message::Lose]
        .iter()
        .map(|msg| (*msg, oracle.attest(&event, *msg)))
        .collect::<HashMap<_, _>>();

    maker_cfd_txs
        .cets
        .into_iter()
        .zip(taker_cfd_txs.cets)
        .try_for_each(|((cet, maker_encsig, msg), (_, taker_encsig, _))| {
            let oracle_sig = attestations
                .get(&msg)
                .expect("oracle to sign all messages in test");
            let (_nonce_pk, signature_scalar) = schnorrsig_decompose(oracle_sig);

            let maker_sig = maker_encsig
                .decrypt(&signature_scalar)
                .context("could not decrypt maker encsig on cet")?;
            let taker_sig = taker_encsig
                .decrypt(&signature_scalar)
                .context("could not decrypt taker encsig on cet")?;

            let signed_cet = finalize_spend_transaction(
                cet,
                &commit_descriptor,
                (maker_pk, maker_sig),
                (taker_pk, taker_sig),
            )?;

            check_tx_fee(&[&signed_commit_tx], &signed_cet).expect("correct fees for cet");

            commit_descriptor
                .address(Network::Regtest)
                .expect("can derive address from descriptor")
                .script_pubkey()
                .verify(
                    0,
                    commit_amount.as_sat(),
                    bitcoin::consensus::serialize(&signed_cet).as_slice(),
                )
                .context("failed to verify cet")
        })
        .expect("all cets to be properly signed");

    // verify punishment transactions

    let punish_tx = punish_transaction(
        &commit_descriptor,
        &maker_address,
        maker_cfd_txs.commit.1,
        maker_sk,
        taker_revocation_sk,
        taker_publish_pk,
        &signed_commit_tx,
    )
    .unwrap();

    check_tx_fee(&[&signed_commit_tx], &punish_tx).expect("correct fees for punish tx");

    commit_descriptor
        .address(Network::Regtest)
        .expect("can derive address from descriptor")
        .script_pubkey()
        .verify(
            0,
            commit_amount.as_sat(),
            bitcoin::consensus::serialize(&punish_tx).as_slice(),
        )
        .expect("valid punish transaction signed by maker");

    let punish_tx = punish_transaction(
        &commit_descriptor,
        &taker_address,
        taker_cfd_txs.commit.1,
        taker_sk,
        maker_revocation_sk,
        maker_publish_pk,
        &signed_commit_tx,
    )
    .unwrap();

    commit_descriptor
        .address(Network::Regtest)
        .expect("can derive address from descriptor")
        .script_pubkey()
        .verify(
            0,
            commit_amount.as_sat(),
            bitcoin::consensus::serialize(&punish_tx).as_slice(),
        )
        .expect("valid punish transaction signed by taker");
}

fn check_tx_fee(input_txs: &[&Transaction], spend_tx: &Transaction) -> Result<()> {
    let input_amount = spend_tx
        .input
        .iter()
        .try_fold::<_, _, Result<_>>(0, |acc, input| {
            let value = input_txs
                .iter()
                .find_map(|tx| {
                    (tx.txid() == input.previous_output.txid)
                        .then(|| tx.output[input.previous_output.vout as usize].value)
                })
                .with_context(|| {
                    format!(
                        "spend tx input {} not found in input_txs",
                        input.previous_output
                    )
                })
                .context("foo")?;

            Ok(acc + value)
        })?;

    let output_amount = spend_tx
        .output
        .iter()
        .fold(0, |acc, output| acc + output.value);
    let fee = input_amount - output_amount;

    let min_relay_fee = spend_tx.get_virtual_size();
    if (fee as f64) < min_relay_fee {
        bail!("min relay fee not met, {} < {}", fee, min_relay_fee)
    }

    Ok(())
}

fn build_wallet<R>(
    rng: &mut R,
    utxo_amount: Amount,
    num_utxos: u8,
) -> Result<bdk::Wallet<(), bdk::database::MemoryDatabase>>
where
    R: RngCore + CryptoRng,
{
    // TODO: Consider upstreaming these imports to be included in the macro.
    use bdk::bitcoin::OutPoint;
    use bdk::{
        miniscript, populate_test_db, testutils, ConfirmationTime, KeychainKind, LocalUtxo,
        TransactionDetails,
    };
    use std::str::FromStr;

    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);

    let key = ExtendedPrivKey::new_master(Network::Regtest, &seed)?;
    let descriptors = testutils!(@descriptors (&format!("wpkh({}/*)", key)));

    let mut database = bdk::database::MemoryDatabase::new();

    for index in 0..num_utxos {
        populate_test_db!(
            &mut database,
            testutils! {
                @tx ( (@external descriptors, index as u32) => utxo_amount.as_sat() ) (@confirmations 1)
            },
            Some(100)
        );
    }

    let wallet = bdk::Wallet::new_offline(&descriptors.0, None, Network::Regtest, database)?;

    Ok(wallet)
}

struct Oracle {
    key_pair: schnorrsig::KeyPair,
}

impl Oracle {
    fn new<R>(rng: &mut R) -> Self
    where
        R: RngCore + CryptoRng,
    {
        let key_pair = schnorrsig::KeyPair::new(SECP256K1, rng);

        Self { key_pair }
    }

    fn public_key(&self) -> schnorrsig::PublicKey {
        schnorrsig::PublicKey::from_keypair(SECP256K1, &self.key_pair)
    }

    fn attest(&self, event: &Event, msg: Message) -> schnorrsig::Signature {
        secp_utils::schnorr_sign_with_nonce(&msg.into(), &self.key_pair, &event.nonce)
    }
}

fn announce<R>(rng: &mut R) -> (Event, Announcement)
where
    R: RngCore + CryptoRng,
{
    let event = Event::new(rng);
    let announcement = event.announcement();

    (event, announcement)
}

/// Represents the oracle's commitment to a nonce that will be used to
/// sign a specific event in the future.
struct Event {
    /// Nonce.
    ///
    /// Must remain secret.
    nonce: SecretKey,
    nonce_pk: schnorrsig::PublicKey,
}

impl Event {
    fn new<R>(rng: &mut R) -> Self
    where
        R: RngCore + CryptoRng,
    {
        let nonce = SecretKey::new(rng);

        let key_pair = schnorrsig::KeyPair::from_secret_key(SECP256K1, nonce);
        let nonce_pk = schnorrsig::PublicKey::from_keypair(SECP256K1, &key_pair);

        Self { nonce, nonce_pk }
    }

    fn announcement(&self) -> Announcement {
        Announcement {
            nonce_pk: self.nonce_pk,
        }
    }
}

/// Public message which can be used by anyone to perform a DLC
/// protocol based on a specific event.
///
/// These would normally include more information to identify the
/// specific event, but we omit this for simplicity. See:
/// https://github.com/discreetlogcontracts/dlcspecs/blob/master/Oracle.md#oracle-events
#[derive(Clone, Copy)]
struct Announcement {
    nonce_pk: schnorrsig::PublicKey,
}

impl Announcement {
    fn nonce_pk(&self) -> schnorrsig::PublicKey {
        self.nonce_pk
    }
}

fn make_keypair<R>(rng: &mut R) -> (SecretKey, PublicKey)
where
    R: RngCore + CryptoRng,
{
    let sk = SecretKey::new(rng);
    let pk = PublicKey::from_private_key(
        SECP256K1,
        &PrivateKey {
            compressed: true,
            network: Network::Regtest,
            key: sk,
        },
    );

    (sk, pk)
}

/// Decompose a BIP340 signature into R and s.
pub fn schnorrsig_decompose(
    signature: &schnorrsig::Signature,
) -> (schnorrsig::PublicKey, SecretKey) {
    let bytes = signature.as_ref();

    let nonce_pk = schnorrsig::PublicKey::from_slice(&bytes[0..32]).expect("R value in sig");
    let s = SecretKey::from_slice(&bytes[32..64]).expect("s value in sig");

    (nonce_pk, s)
}

mod secp_utils {
    use super::*;

    use secp256k1_zkp::secp256k1_zkp_sys::types::c_void;
    use secp256k1_zkp::secp256k1_zkp_sys::CPtr;
    use std::os::raw::{c_int, c_uchar};
    use std::ptr;

    /// Create a Schnorr signature using the provided nonce instead of generating one.
    pub fn schnorr_sign_with_nonce(
        msg: &secp256k1_zkp::Message,
        keypair: &schnorrsig::KeyPair,
        nonce: &SecretKey,
    ) -> schnorrsig::Signature {
        unsafe {
            let mut sig = [0u8; secp256k1_zkp::constants::SCHNORRSIG_SIGNATURE_SIZE];
            assert_eq!(
                1,
                secp256k1_zkp::ffi::secp256k1_schnorrsig_sign(
                    *SECP256K1.ctx(),
                    sig.as_mut_c_ptr(),
                    msg.as_c_ptr(),
                    keypair.as_ptr(),
                    Some(constant_nonce_fn),
                    nonce.as_c_ptr() as *const c_void
                )
            );

            schnorrsig::Signature::from_slice(&sig).unwrap()
        }
    }

    extern "C" fn constant_nonce_fn(
        nonce32: *mut c_uchar,
        _msg32: *const c_uchar,
        _key32: *const c_uchar,
        _xonly_pk32: *const c_uchar,
        _algo16: *const c_uchar,
        data: *mut c_void,
    ) -> c_int {
        unsafe {
            ptr::copy_nonoverlapping(data as *const c_uchar, nonce32, 32);
        }
        1
    }
}
