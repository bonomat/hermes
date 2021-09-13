use anyhow::{bail, Context, Result};
use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use bdk::bitcoin::{Address, Amount, Network, PrivateKey, PublicKey, Transaction};
use bdk::descriptor::Descriptor;
use bdk::miniscript::DescriptorTrait;
use bdk::wallet::AddressIndex;
use bdk::SignOptions;
use bitcoin::util::psbt::PartiallySignedTransaction;
use bitcoin::Txid;
use cfd_protocol::{
    commit_descriptor, compute_signature_point, create_cfd_transactions,
    finalize_spend_transaction, lock_descriptor, punish_transaction, renew_cfd_transactions,
    spending_tx_sighash, CfdTransactions, Payout, PunishParams, TransactionExt, WalletExt,
};
use rand::{CryptoRng, RngCore, SeedableRng};
use rand_chacha::ChaChaRng;
use secp256k1_zkp::bitcoin_hashes::sha256;
use secp256k1_zkp::{schnorrsig, EcdsaAdaptorSignature, SecretKey, Signature, SECP256K1};

#[test]
fn create_cfd() {
    let mut rng = ChaChaRng::seed_from_u64(0);

    let maker_lock_amount = Amount::ONE_BTC;
    let taker_lock_amount = Amount::ONE_BTC;

    let maker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();
    let taker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();

    let oracle = Oracle::new(&mut rng);
    let (event, announcement) = announce(&mut rng);

    let payouts = vec![
        Payout::new(
            b"win".to_vec(),
            announcement.nonce_pk(),
            Amount::from_btc(1.5).unwrap(),
            Amount::from_btc(0.5).unwrap(),
        ),
        Payout::new(
            b"lose".to_vec(),
            announcement.nonce_pk(),
            Amount::ZERO,
            Amount::from_btc(2.0).unwrap(),
        ),
    ];

    let refund_timelock = 0;

    let (maker_cfd_txs, taker_cfd_txs, maker, taker, maker_addr, taker_addr) = create_cfd_txs(
        &mut rng,
        (&maker_wallet, maker_lock_amount),
        (&taker_wallet, taker_lock_amount),
        oracle.public_key(),
        payouts,
        refund_timelock,
    );

    let lock_desc = lock_descriptor(maker.pk, taker.pk);
    let lock_amount = maker_lock_amount + taker_lock_amount;

    let commit_desc = commit_descriptor(
        (maker.pk, maker.rev_pk, maker.pub_pk),
        (taker.pk, taker.rev_pk, taker.pub_pk),
    );
    let commit_amount = Amount::from_sat(maker_cfd_txs.commit.0.output[0].value);

    verify_cfd_sigs(
        (&maker_cfd_txs, maker.pk, maker.pub_pk),
        (&taker_cfd_txs, taker.pk, taker.pub_pk),
        oracle.public_key(),
        (&lock_desc, lock_amount),
        (&commit_desc, commit_amount),
    );

    check_cfd_txs(
        (
            maker_wallet,
            maker_cfd_txs,
            maker.sk,
            maker.pk,
            maker.pub_sk,
            maker.pub_pk,
            maker.rev_sk,
            maker_addr,
        ),
        (
            taker_wallet,
            taker_cfd_txs,
            taker.sk,
            taker.pk,
            taker.pub_sk,
            taker.pub_pk,
            taker.rev_sk,
            taker_addr,
        ),
        (oracle, event),
        (lock_desc, lock_amount),
        (commit_desc, commit_amount),
    );
}

#[test]
fn renew_cfd() {
    let mut rng = ChaChaRng::seed_from_u64(0);

    let maker_lock_amount = Amount::ONE_BTC;
    let taker_lock_amount = Amount::ONE_BTC;

    let maker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();
    let taker_wallet = build_wallet(&mut rng, Amount::from_btc(0.4).unwrap(), 5).unwrap();

    let oracle = Oracle::new(&mut rng);
    let (_event, announcement) = announce(&mut rng);

    let payouts = vec![
        Payout::new(
            b"win".to_vec(),
            announcement.nonce_pk(),
            Amount::from_btc(2.0).unwrap(),
            Amount::ZERO,
        ),
        Payout::new(
            b"lose".to_vec(),
            announcement.nonce_pk(),
            Amount::ZERO,
            Amount::from_btc(2.0).unwrap(),
        ),
    ];

    let refund_timelock = 0;

    let (maker_cfd_txs, taker_cfd_txs, maker, taker, maker_addr, taker_addr) = create_cfd_txs(
        &mut rng,
        (&maker_wallet, maker_lock_amount),
        (&taker_wallet, taker_lock_amount),
        oracle.public_key(),
        payouts,
        refund_timelock,
    );

    // renew cfd transactions

    let (maker_rev_sk, maker_rev_pk) = make_keypair(&mut rng);
    let (maker_pub_sk, maker_pub_pk) = make_keypair(&mut rng);

    let (taker_rev_sk, taker_rev_pk) = make_keypair(&mut rng);
    let (taker_pub_sk, taker_pub_pk) = make_keypair(&mut rng);

    let (event, announcement) = announce(&mut rng);

    let payouts = vec![
        Payout::new(
            b"win".to_vec(),
            announcement.nonce_pk(),
            Amount::from_btc(1.5).unwrap(),
            Amount::from_btc(0.5).unwrap(),
        ),
        Payout::new(
            b"lose".to_vec(),
            announcement.nonce_pk(),
            Amount::from_btc(0.5).unwrap(),
            Amount::from_btc(1.5).unwrap(),
        ),
    ];

    let maker_cfd_txs = renew_cfd_transactions(
        maker_cfd_txs.lock,
        (
            maker.pk,
            maker_lock_amount,
            maker_addr.clone(),
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker.pk,
            taker_lock_amount,
            taker_addr.clone(),
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        oracle.public_key(),
        refund_timelock,
        payouts.clone(),
        maker.sk,
    )
    .unwrap();

    let taker_cfd_txs = renew_cfd_transactions(
        taker_cfd_txs.lock,
        (
            maker.pk,
            maker_lock_amount,
            maker_addr.clone(),
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker.pk,
            taker_lock_amount,
            taker_addr.clone(),
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        oracle.public_key(),
        refund_timelock,
        payouts,
        taker.sk,
    )
    .unwrap();

    let lock_desc = lock_descriptor(maker.pk, taker.pk);
    let lock_amount = maker_lock_amount + taker_lock_amount;

    let commit_desc = commit_descriptor(
        (maker.pk, maker_rev_pk, maker_pub_pk),
        (taker.pk, taker_rev_pk, taker_pub_pk),
    );
    let commit_amount = Amount::from_sat(maker_cfd_txs.commit.0.output[0].value);

    verify_cfd_sigs(
        (&maker_cfd_txs, maker.pk, maker_pub_pk),
        (&taker_cfd_txs, taker.pk, taker_pub_pk),
        oracle.public_key(),
        (&lock_desc, lock_amount),
        (&commit_desc, commit_amount),
    );

    check_cfd_txs(
        (
            maker_wallet,
            maker_cfd_txs,
            maker.sk,
            maker.pk,
            maker_pub_sk,
            maker_pub_pk,
            maker_rev_sk,
            maker_addr,
        ),
        (
            taker_wallet,
            taker_cfd_txs,
            taker.sk,
            taker.pk,
            taker_pub_sk,
            taker_pub_pk,
            taker_rev_sk,
            taker_addr,
        ),
        (oracle, event),
        (lock_desc, lock_amount),
        (commit_desc, commit_amount),
    )
}

fn create_cfd_txs(
    rng: &mut ChaChaRng,
    (maker_wallet, maker_lock_amount): (&bdk::Wallet<(), bdk::database::MemoryDatabase>, Amount),
    (taker_wallet, taker_lock_amount): (&bdk::Wallet<(), bdk::database::MemoryDatabase>, Amount),
    oracle_pk: schnorrsig::PublicKey,
    payouts: Vec<Payout>,
    refund_timelock: u32,
) -> (
    CfdTransactions,
    CfdTransactions,
    CfdKeys,
    CfdKeys,
    Address,
    Address,
) {
    let (maker_sk, maker_pk) = make_keypair(rng);
    let (taker_sk, taker_pk) = make_keypair(rng);

    let maker_addr = maker_wallet.get_address(AddressIndex::New).unwrap();
    let taker_addr = taker_wallet.get_address(AddressIndex::New).unwrap();

    let (maker_rev_sk, maker_rev_pk) = make_keypair(rng);
    let (maker_pub_sk, maker_pub_pk) = make_keypair(rng);
    let (taker_rev_sk, taker_rev_pk) = make_keypair(rng);
    let (taker_pub_sk, taker_pub_pk) = make_keypair(rng);
    let maker_params = maker_wallet
        .build_party_params(maker_lock_amount, maker_pk)
        .unwrap();
    let taker_params = taker_wallet
        .build_party_params(taker_lock_amount, taker_pk)
        .unwrap();
    let maker_cfd_txs = create_cfd_transactions(
        (
            maker_params.clone(),
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker_params.clone(),
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        oracle_pk,
        refund_timelock,
        payouts.clone(),
        maker_sk,
    )
    .unwrap();
    let taker_cfd_txs = create_cfd_transactions(
        (
            maker_params,
            PunishParams {
                revocation_pk: maker_rev_pk,
                publish_pk: maker_pub_pk,
            },
        ),
        (
            taker_params,
            PunishParams {
                revocation_pk: taker_rev_pk,
                publish_pk: taker_pub_pk,
            },
        ),
        oracle_pk,
        refund_timelock,
        payouts,
        taker_sk,
    )
    .unwrap();
    (
        maker_cfd_txs,
        taker_cfd_txs,
        CfdKeys {
            sk: maker_sk,
            pk: maker_pk,
            rev_sk: maker_rev_sk,
            rev_pk: maker_rev_pk,
            pub_sk: maker_pub_sk,
            pub_pk: maker_pub_pk,
        },
        CfdKeys {
            sk: taker_sk,
            pk: taker_pk,
            rev_sk: taker_rev_sk,
            rev_pk: taker_rev_pk,
            pub_sk: taker_pub_sk,
            pub_pk: taker_pub_pk,
        },
        maker_addr.address,
        taker_addr.address,
    )
}

struct CfdKeys {
    sk: SecretKey,
    pk: PublicKey,
    rev_sk: SecretKey,
    rev_pk: PublicKey,
    pub_sk: SecretKey,
    pub_pk: PublicKey,
}

fn verify_cfd_sigs(
    (maker_cfd_txs, maker_pk, maker_publish_pk): (&CfdTransactions, PublicKey, PublicKey),
    (taker_cfd_txs, taker_pk, taker_publish_pk): (&CfdTransactions, PublicKey, PublicKey),
    oracle_pk: schnorrsig::PublicKey,
    (lock_desc, lock_amount): (&Descriptor<PublicKey>, Amount),
    (commit_desc, commit_amount): (&Descriptor<PublicKey>, Amount),
) {
    verify_spend(
        &taker_cfd_txs.refund.0,
        &maker_cfd_txs.refund.1,
        commit_desc,
        commit_amount,
        &maker_pk.key,
    )
    .expect("valid maker refund sig");
    verify_spend(
        &maker_cfd_txs.refund.0,
        &taker_cfd_txs.refund.1,
        commit_desc,
        commit_amount,
        &taker_pk.key,
    )
    .expect("valid taker refund sig");
    for (tx, _, msg, nonce_pk) in taker_cfd_txs.cets.iter() {
        let maker_encsig = maker_cfd_txs
            .cets
            .iter()
            .find_map(|(maker_tx, encsig, _, _)| (maker_tx.txid() == tx.txid()).then(|| encsig))
            .expect("one encsig per cet, per party");

        verify_cet_encsig(
            tx,
            maker_encsig,
            msg,
            &maker_pk.key,
            (&oracle_pk, nonce_pk),
            commit_desc,
            commit_amount,
        )
        .expect("valid maker cet encsig")
    }
    for (tx, _, msg, nonce_pk) in maker_cfd_txs.cets.iter() {
        let taker_encsig = taker_cfd_txs
            .cets
            .iter()
            .find_map(|(taker_tx, encsig, _, _)| (taker_tx.txid() == tx.txid()).then(|| encsig))
            .expect("one encsig per cet, per party");

        verify_cet_encsig(
            tx,
            taker_encsig,
            msg,
            &taker_pk.key,
            (&oracle_pk, nonce_pk),
            commit_desc,
            commit_amount,
        )
        .expect("valid taker cet encsig")
    }
    encverify_spend(
        &taker_cfd_txs.commit.0,
        &maker_cfd_txs.commit.1,
        lock_desc,
        lock_amount,
        &taker_publish_pk.key,
        &maker_pk.key,
    )
    .expect("valid maker commit encsig");
    encverify_spend(
        &maker_cfd_txs.commit.0,
        &taker_cfd_txs.commit.1,
        lock_desc,
        lock_amount,
        &maker_publish_pk.key,
        &taker_pk.key,
    )
    .expect("valid taker commit encsig");
}

fn check_cfd_txs(
    (
        maker_wallet,
        maker_cfd_txs,
        maker_sk,
        maker_pk,
        maker_pub_sk,
        maker_pub_pk,
        maker_rev_sk,
        maker_addr,
    ): (
        bdk::Wallet<(), bdk::database::MemoryDatabase>,
        CfdTransactions,
        SecretKey,
        PublicKey,
        SecretKey,
        PublicKey,
        SecretKey,
        Address,
    ),
    (
        taker_wallet,
        taker_cfd_txs,
        taker_sk,
        taker_pk,
        taker_pub_sk,
        taker_pub_pk,
        taker_rev_sk,
        taker_addr,
    ): (
        bdk::Wallet<(), bdk::database::MemoryDatabase>,
        CfdTransactions,
        SecretKey,
        PublicKey,
        SecretKey,
        PublicKey,
        SecretKey,
        Address,
    ),
    (oracle, event): (Oracle, Event),
    (lock_desc, lock_amount): (Descriptor<PublicKey>, Amount),
    (commit_desc, commit_amount): (Descriptor<PublicKey>, Amount),
) {
    // Lock transaction (either party can do this):

    let signed_lock_tx = sign_lock_tx(maker_cfd_txs.lock, maker_wallet, taker_wallet)
        .expect("to build signed lock tx");

    // Commit transactions:

    let signed_commit_tx_maker = decrypt_and_sign(
        maker_cfd_txs.commit.0,
        (&maker_sk, &maker_pk),
        &maker_pub_sk,
        &taker_pk,
        &taker_cfd_txs.commit.1,
        &lock_desc,
        lock_amount,
    )
    .expect("maker to build signed commit tx");
    check_tx(&signed_lock_tx, &signed_commit_tx_maker, &lock_desc).expect("valid maker commit tx");
    let signed_commit_tx_taker = decrypt_and_sign(
        taker_cfd_txs.commit.0,
        (&taker_sk, &taker_pk),
        &taker_pub_sk,
        &maker_pk,
        &maker_cfd_txs.commit.1,
        &lock_desc,
        lock_amount,
    )
    .expect("taker to build signed commit tx");
    check_tx(&signed_lock_tx, &signed_commit_tx_taker, &lock_desc).expect("valid taker commit tx");

    // Refund transaction (both parties would produce the same one):

    let signed_refund_tx = finalize_spend_transaction(
        maker_cfd_txs.refund.0,
        &commit_desc,
        (maker_pk, maker_cfd_txs.refund.1),
        (taker_pk, taker_cfd_txs.refund.1),
    )
    .expect("to build signed refund tx");
    check_tx(&signed_commit_tx_maker, &signed_refund_tx, &commit_desc).expect("valid refund tx");

    // CETs:

    for (tx, _, msg, _) in maker_cfd_txs.cets.clone().into_iter() {
        build_and_check_cet(
            tx,
            &oracle.attest(&event, &msg),
            taker_cfd_txs
                .cets
                .iter()
                .map(|(tx, encsig, _, _)| (tx.txid(), *encsig)),
            (&maker_sk, &maker_pk),
            &taker_pk,
            (&signed_commit_tx_maker, &commit_desc, commit_amount),
        )
        .expect("valid maker cet");
    }
    for (tx, _, msg, _) in taker_cfd_txs.cets.into_iter() {
        build_and_check_cet(
            tx,
            &oracle.attest(&event, &msg),
            maker_cfd_txs
                .cets
                .iter()
                .map(|(tx, encsig, _, _)| (tx.txid(), *encsig)),
            (&taker_sk, &taker_pk),
            &maker_pk,
            (&signed_commit_tx_maker, &commit_desc, commit_amount),
        )
        .expect("valid taker cet");
    }

    // Punish transactions:

    let punish_tx_maker = punish_transaction(
        &commit_desc,
        &maker_addr,
        maker_cfd_txs.commit.1,
        maker_sk,
        taker_rev_sk,
        taker_pub_pk,
        &signed_commit_tx_taker,
    )
    .expect("maker to build punish tx");
    check_tx(&signed_commit_tx_taker, &punish_tx_maker, &commit_desc)
        .expect("valid maker punish tx");
    let punish_tx_taker = punish_transaction(
        &commit_desc,
        &taker_addr,
        taker_cfd_txs.commit.1,
        taker_sk,
        maker_rev_sk,
        maker_pub_pk,
        &signed_commit_tx_maker,
    )
    .expect("taker to build punish tx");
    check_tx(&signed_commit_tx_maker, &punish_tx_taker, &commit_desc)
        .expect("valid taker punish tx");
}

fn build_and_check_cet(
    cet: Transaction,
    oracle_sig: &schnorrsig::Signature,
    mut cets_other: impl Iterator<Item = (Txid, EcdsaAdaptorSignature)>,
    (maker_sk, maker_pk): (&SecretKey, &PublicKey),
    taker_pk: &PublicKey,
    (commit_tx, commit_desc, commit_amount): (&Transaction, &Descriptor<PublicKey>, Amount),
) -> Result<()> {
    let (_nonce_pk, signature_scalar) = schnorrsig_decompose(oracle_sig);
    let taker_encsig = cets_other
        .find_map(|(txid, encsig)| (txid == cet.txid()).then(|| encsig))
        .expect("one encsig per cet, per party");
    let signed_cet = decrypt_and_sign(
        cet,
        (maker_sk, maker_pk),
        &signature_scalar,
        taker_pk,
        &taker_encsig,
        commit_desc,
        commit_amount,
    )
    .context("failed to build signed cet")?;
    check_tx(commit_tx, &signed_cet, commit_desc).context("invalid cet")?;

    Ok(())
}

fn check_tx(
    spent_tx: &Transaction,
    spend_tx: &Transaction,
    spent_descriptor: &Descriptor<PublicKey>,
) -> Result<()> {
    let spent_script_pubkey = spent_descriptor.script_pubkey();
    let spent_outpoint = spent_tx
        .outpoint(&spent_script_pubkey)
        .context("spend tx doesn't spend from spent tx")?;
    let spent_amount = spent_tx.output[spent_outpoint.vout as usize].value;

    check_tx_fee(&[spent_tx], spend_tx)?;
    spent_descriptor.script_pubkey().verify(
        0,
        spent_amount,
        bitcoin::consensus::serialize(spend_tx).as_slice(),
    )?;

    Ok(())
}

fn decrypt_and_sign(
    spend_tx: Transaction,
    (sk, pk): (&SecretKey, &PublicKey),
    decryption_sk: &SecretKey,
    pk_other: &PublicKey,
    encsig_other: &EcdsaAdaptorSignature,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
) -> Result<Transaction> {
    let sighash = spending_tx_sighash(&spend_tx, spent_descriptor, spent_amount);

    let sig_self = SECP256K1.sign(&sighash, sk);
    let sig_other = encsig_other.decrypt(decryption_sk)?;

    let signed_commit_tx = finalize_spend_transaction(
        spend_tx,
        spent_descriptor,
        (*pk, sig_self),
        (*pk_other, sig_other),
    )?;

    Ok(signed_commit_tx)
}

fn sign_lock_tx(
    mut lock_tx: PartiallySignedTransaction,
    maker_wallet: bdk::Wallet<(), bdk::database::MemoryDatabase>,
    taker_wallet: bdk::Wallet<(), bdk::database::MemoryDatabase>,
) -> Result<Transaction> {
    maker_wallet
        .sign(
            &mut lock_tx,
            SignOptions {
                trust_witness_utxo: true,
                ..Default::default()
            },
        )
        .context("maker could not sign lock tx")?;
    taker_wallet
        .sign(
            &mut lock_tx,
            SignOptions {
                trust_witness_utxo: true,
                ..Default::default()
            },
        )
        .context("taker could not sign lock tx")?;

    Ok(lock_tx.extract_tx())
}

fn verify_spend(
    tx: &Transaction,
    sig: &Signature,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
    pk: &secp256k1_zkp::PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount);
    SECP256K1
        .verify(&sighash, sig, pk)
        .context("failed to verify sig on spend tx")
}

fn verify_cet_encsig(
    tx: &Transaction,
    encsig: &EcdsaAdaptorSignature,
    msg: &[u8],
    pk: &secp256k1_zkp::PublicKey,
    (oracle_pk, nonce_pk): (&schnorrsig::PublicKey, &schnorrsig::PublicKey),
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
) -> Result<()> {
    let sig_point = compute_signature_point(oracle_pk, nonce_pk, msg)
        .context("could not calculate signature point")?;
    encverify_spend(tx, encsig, spent_descriptor, spent_amount, &sig_point, pk)
}

fn encverify_spend(
    tx: &Transaction,
    encsig: &EcdsaAdaptorSignature,
    spent_descriptor: &Descriptor<PublicKey>,
    spent_amount: Amount,
    encryption_point: &secp256k1_zkp::PublicKey,
    pk: &secp256k1_zkp::PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount);
    encsig
        .verify(SECP256K1, &sighash, pk, encryption_point)
        .context("failed to verify encsig spend tx")
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
                })?;

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
    use bdk::{populate_test_db, testutils};

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

    fn attest(&self, event: &Event, msg: &[u8]) -> schnorrsig::Signature {
        let msg = secp256k1_zkp::Message::from_hashed_data::<sha256::Hash>(msg);

        secp_utils::schnorr_sign_with_nonce(&msg, &self.key_pair, &event.nonce)
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
fn schnorrsig_decompose(signature: &schnorrsig::Signature) -> (schnorrsig::PublicKey, SecretKey) {
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
