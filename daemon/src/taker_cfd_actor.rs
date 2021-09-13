use crate::model::cfd::{Cfd, CfdOffer, CfdOfferId, CfdState, CfdStateCommon};
use crate::model::Usd;
use crate::wire::{Msg0, Msg1, SetupMsg};
use crate::{db, wire};
use bdk::bitcoin::secp256k1::{schnorrsig, SecretKey, Signature};
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::{self, Amount, Transaction};
use bdk::database::BatchDatabase;
use cfd_protocol::{
    commit_descriptor, create_cfd_transactions, lock_descriptor, EcdsaAdaptorSignature,
    PartyParams, PunishParams, WalletExt,
};
use core::panic;
use futures::Future;
use std::collections::HashMap;
use std::time::SystemTime;
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    TakeOffer { offer_id: CfdOfferId, quantity: Usd },
    NewOffer(Option<CfdOffer>),
    OfferAccepted(CfdOfferId),
    IncProtocolMsg(SetupMsg),
    CfdSetupCompleted(FinalizedCfd),
}

pub fn new<B, D>(
    db: sqlx::SqlitePool,
    wallet: bdk::Wallet<B, D>,
    oracle_pk: schnorrsig::PublicKey,
    cfd_feed_actor_inbox: watch::Sender<Vec<Cfd>>,
    offer_feed_actor_inbox: watch::Sender<Option<CfdOffer>>,
    out_msg_maker_inbox: mpsc::UnboundedSender<wire::TakerToMaker>,
) -> (impl Future<Output = ()>, mpsc::UnboundedSender<Command>)
where
    D: BatchDatabase,
{
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut current_contract_setup = None;

    let actor = {
        let sender = sender.clone();

        async move {
            while let Some(message) = receiver.recv().await {
                match message {
                    Command::TakeOffer { offer_id, quantity } => {
                        let mut conn = db.acquire().await.unwrap();

                        let current_offer =
                            db::load_offer_by_id(offer_id, &mut conn).await.unwrap();

                        println!("Accepting current offer: {:?}", &current_offer);

                        let cfd = Cfd::new(
                            current_offer,
                            quantity,
                            CfdState::PendingTakeRequest {
                                common: CfdStateCommon {
                                    transition_timestamp: SystemTime::now(),
                                },
                            },
                            Usd::ZERO,
                        )
                        .unwrap();

                        db::insert_cfd(cfd, &mut conn).await.unwrap();

                        cfd_feed_actor_inbox
                            .send(db::load_all_cfds(&mut conn).await.unwrap())
                            .unwrap();
                        out_msg_maker_inbox
                            .send(wire::TakerToMaker::TakeOffer { offer_id, quantity })
                            .unwrap();
                    }
                    Command::NewOffer(Some(offer)) => {
                        let mut conn = db.acquire().await.unwrap();
                        db::insert_cfd_offer(&offer, &mut conn).await.unwrap();
                        offer_feed_actor_inbox.send(Some(offer)).unwrap();
                    }
                    Command::NewOffer(None) => {
                        offer_feed_actor_inbox.send(None).unwrap();
                    }
                    Command::OfferAccepted(offer_id) => {
                        let mut conn = db.acquire().await.unwrap();
                        db::insert_new_cfd_state_by_offer_id(
                            offer_id,
                            CfdState::ContractSetup {
                                common: CfdStateCommon {
                                    transition_timestamp: SystemTime::now(),
                                },
                            },
                            &mut conn,
                        )
                        .await
                        .unwrap();

                        cfd_feed_actor_inbox
                            .send(db::load_all_cfds(&mut conn).await.unwrap())
                            .unwrap();

                        let (sk, pk) = crate::keypair::new(&mut rand::thread_rng());

                        let taker_params = wallet
                            .build_party_params(bitcoin::Amount::ZERO, pk) // TODO: Load correct quantity from DB
                            .unwrap();

                        let (actor, inbox) = setup_contract(
                            {
                                let inbox = out_msg_maker_inbox.clone();

                                move |msg| inbox.send(wire::TakerToMaker::Protocol(msg)).unwrap()
                            },
                            taker_params,
                            sk,
                            oracle_pk,
                        );

                        tokio::spawn({
                            let sender = sender.clone();

                            async move {
                                sender
                                    .send(Command::CfdSetupCompleted(actor.await))
                                    .unwrap()
                            }
                        });
                        current_contract_setup = Some(inbox);
                    }
                    Command::IncProtocolMsg(msg) => {
                        let inbox = match &current_contract_setup {
                            None => panic!("whoops"),
                            Some(inbox) => inbox,
                        };

                        inbox.send(msg).unwrap();
                    }
                    Command::CfdSetupCompleted(_finalized_cfd) => {
                        todo!("but what?")
                    }
                }
            }
        }
    };

    (actor, sender)
}

/// Contains all data we've assembled about the CFD through the setup protocol.
///
/// All contained signatures are the signatures of THE OTHER PARTY.
/// To use any of these transactions, we need to re-sign them with the correct secret key.
#[derive(Debug)]
pub struct FinalizedCfd {
    pub identity: SecretKey,
    pub revocation: SecretKey,
    pub publish: SecretKey,

    pub lock: PartiallySignedTransaction,
    pub commit: (Transaction, EcdsaAdaptorSignature),
    pub cets: Vec<(Transaction, EcdsaAdaptorSignature, Vec<u8>)>,
    pub refund: (Transaction, Signature),
}

/// Given an initial set of parameters, sets up the CFD contract with the maker.
///
/// Returns the [`FinalizedCfd`] which contains the lock transaction, ready to be signed and sent to
/// the maker. Signing of the lock transaction is not included in this function because we want the
/// actor above to own the wallet.
fn setup_contract(
    send_to_maker: impl Fn(SetupMsg),
    taker: PartyParams,
    sk: SecretKey,
    oracle_pk: schnorrsig::PublicKey,
) -> (
    impl Future<Output = FinalizedCfd>,
    mpsc::UnboundedSender<SetupMsg>,
) {
    let (sender, mut receiver) = mpsc::unbounded_channel::<SetupMsg>();

    let actor = async move {
        let (rev_sk, rev_pk) = crate::keypair::new(&mut rand::thread_rng());
        let (publish_sk, publish_pk) = crate::keypair::new(&mut rand::thread_rng());

        let taker_punish = PunishParams {
            revocation_pk: rev_pk,
            publish_pk,
        };
        send_to_maker(SetupMsg::Msg0(Msg0::from((taker.clone(), taker_punish))));

        let msg0 = receiver.recv().await.unwrap().try_into_msg0().unwrap();
        let (maker, maker_punish) = msg0.into();

        let taker_cfd_txs = create_cfd_transactions(
            (maker.clone(), maker_punish),
            (taker.clone(), taker_punish),
            oracle_pk,
            0, // TODO: Calculate refund timelock based on CFD term
            vec![],
            sk,
        )
        .unwrap();

        send_to_maker(SetupMsg::Msg1(Msg1::from(taker_cfd_txs.clone())));
        let msg1 = receiver.recv().await.unwrap().try_into_msg1().unwrap();

        let _lock_desc = lock_descriptor(maker.identity_pk, taker.identity_pk);
        // let lock_amount = maker_lock_amount + taker_lock_amount;

        let _commit_desc = commit_descriptor(
            (
                maker.identity_pk,
                maker_punish.revocation_pk,
                maker_punish.publish_pk,
            ),
            (taker.identity_pk, rev_pk, publish_pk),
        );
        let commit_tx = taker_cfd_txs.commit.0;

        let _commit_amount = Amount::from_sat(commit_tx.output[0].value);

        // TODO: Verify all signatures from the maker here

        let lock_tx = taker_cfd_txs.lock;
        let refund_tx = taker_cfd_txs.refund.0;

        let mut cet_by_id = taker_cfd_txs
            .cets
            .into_iter()
            .map(|(tx, _, msg, _)| (tx.txid(), (tx, msg)))
            .collect::<HashMap<_, _>>();

        FinalizedCfd {
            identity: sk,
            revocation: rev_sk,
            publish: publish_sk,
            lock: lock_tx,
            commit: (commit_tx, msg1.commit),
            cets: msg1
                .cets
                .into_iter()
                .map(|(txid, sig)| {
                    let (cet, msg) = cet_by_id.remove(&txid).expect("unknown CET");

                    (cet, sig, msg)
                })
                .collect::<Vec<_>>(),
            refund: (refund_tx, msg1.refund),
        }
    };

    (actor, sender)
}
