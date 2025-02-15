#![cfg_attr(not(test), warn(clippy::unwrap_used))]

use crate::bitcoin::util::psbt::PartiallySignedTransaction;
use crate::bitcoin::Txid;
use crate::listen_protocols::TAKER_LISTEN_PROTOCOLS;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
pub use bdk;
use bdk::bitcoin;
use bdk::bitcoin::Amount;
use bdk::FeeRate;
use identify::PeerInfo;
use libp2p_core::Multiaddr;
use libp2p_tcp::TokioTcpConfig;
pub use maia;
pub use maia_core;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use model::libp2p::PeerId;
use model::olivia;
use model::Contracts;
use model::Identity;
use model::Leverage;
use model::OfferId;
use model::OrderId;
use model::Price;
use model::Role;
use online_status::ConnectionStatus;
use parse_display::Display;
use ping_pong::ping;
use ping_pong::pong;
use seed::Identities;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use time::ext::NumericalDuration;
use tokio::sync::watch;
use tokio_extras::Tasks;
use tracing::instrument;
use xtra::prelude::*;
use xtra_bitmex_price_feed::QUOTE_INTERVAL_MINUTES;
use xtra_libp2p::dialer;
use xtra_libp2p::endpoint;
use xtra_libp2p::multiaddress_ext::MultiaddrExt;
use xtra_libp2p::Endpoint;
use xtras::supervisor::always_restart_after;
use xtras::supervisor::Supervisor;

pub mod archive_closed_cfds;
pub mod archive_failed_cfds;
pub mod auto_rollover;
pub mod collab_settlement;
pub mod command;
pub mod identify;
pub mod libp2p_utils;
pub mod listen_protocols;
pub mod monitor;
pub mod online_status;
pub mod oracle;
pub mod order;
pub mod position_metrics;
pub mod process_manager;
pub mod projection;
pub mod seed;
pub mod taker_cfd;
pub mod wallet;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Duration between the restart attempts after a supervised actor has quit with
/// a failure.
pub const RESTART_INTERVAL: Duration = Duration::from_secs(5);

pub const ENDPOINT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

pub const N_PAYOUTS: usize = 200;

pub struct TakerActorSystem<O, W, P> {
    pub cfd_actor: Address<taker_cfd::Actor>,
    wallet_actor: Address<W>,
    _oracle_actor: Address<O>,
    pub auto_rollover_actor: Address<auto_rollover::Actor>,
    pub price_feed_actor: Address<P>,
    executor: command::Executor,
    _close_cfds_actor: Address<archive_closed_cfds::Actor>,
    _archive_failed_cfds_actor: Address<archive_failed_cfds::Actor>,
    _pong_actor: Address<pong::Actor>,
    _online_status_actor: Address<online_status::Actor>,
    _identify_dialer_actor: Address<identify::dialer::Actor>,

    pub maker_online_status_feed_receiver: watch::Receiver<ConnectionStatus>,
    pub identify_info_feed_receiver: watch::Receiver<Option<PeerInfo>>,

    _tasks: Tasks,
}

impl<O, W, P> TakerActorSystem<O, W, P>
where
    O: Handler<oracle::MonitorAttestations, Return = ()>
        + Handler<
            oracle::GetAnnouncements,
            Return = Result<Vec<olivia::Announcement>, oracle::NoAnnouncement>,
        > + Actor<Stop = ()>,
    W: Handler<wallet::BuildPartyParams, Return = Result<maia_core::PartyParams>>
        + Handler<wallet::Sign, Return = Result<PartiallySignedTransaction>>
        + Handler<wallet::Withdraw, Return = Result<Txid>>
        + Handler<wallet::Sync, Return = ()>
        + Actor<Stop = ()>,
    P: Handler<
            xtra_bitmex_price_feed::GetLatestQuotes,
            Return = xtra_bitmex_price_feed::LatestQuotes,
        > + Actor<Stop = xtra_bitmex_price_feed::Error>,
{
    #[instrument(
        name = "Create TakerActorSystem",
        skip_all,
        fields(
            %n_payouts,
            connect_timeout_secs = %connect_timeout.as_secs(),
            %environment,
        )
        err,
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn new<M>(
        db: sqlite_db::Connection,
        wallet_actor_addr: Address<W>,
        oracle_pk: XOnlyPublicKey,
        identity: Identities,
        oracle_constructor: impl FnOnce(command::Executor) -> O,
        monitor_constructor: impl FnOnce(command::Executor) -> Result<M>,
        price_feed_actor: Address<P>,
        n_payouts: usize,
        connect_timeout: Duration,
        projection_actor: Address<projection::Actor>,
        maker_identity: Identity,
        maker_multiaddr: Multiaddr,
        environment: Environment,
    ) -> Result<Self>
    where
        M: Handler<monitor::MonitorAfterContractSetup, Return = ()>
            + Handler<monitor::MonitorAfterRollover, Return = ()>
            + Handler<monitor::Sync, Return = ()>
            + Handler<monitor::MonitorCollaborativeSettlement, Return = ()>
            + Handler<monitor::MonitorCetFinality, Return = Result<()>>
            + Handler<monitor::TryBroadcastTransaction, Return = Result<()>>
            + Actor<Stop = ()>,
    {
        let (maker_online_status_feed_sender, maker_online_status_feed_receiver) =
            watch::channel(ConnectionStatus::Offline);

        let (monitor_addr, monitor_ctx) = Context::new(None);
        let (oracle_addr, oracle_ctx) = Context::new(None);
        let (process_manager_addr, process_manager_ctx) = Context::new(None);

        let executor = command::Executor::new(db.clone(), process_manager_addr.clone());

        let mut tasks = Tasks::default();

        let position_metrics_actor = position_metrics::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);

        tasks.add(process_manager_ctx.run(process_manager::Actor::new(
            db.clone(),
            Role::Taker,
            projection_actor.clone().into(),
            position_metrics_actor.into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.into(),
            oracle_addr.clone().into(),
        )));

        let (endpoint_addr, endpoint_context) = Context::new(None);

        let (order_supervisor, order) = Supervisor::new({
            let oracle = oracle_addr.clone();
            let db = db.clone();
            let process_manager = process_manager_addr;
            let wallet = wallet_actor_addr.clone();
            let projection = projection_actor.clone();
            let endpoint = endpoint_addr.clone();
            move || {
                order::taker::Actor::new(
                    n_payouts,
                    oracle_pk,
                    oracle.clone().into(),
                    (db.clone(), process_manager.clone()),
                    (wallet.clone().into(), wallet.clone().into()),
                    projection.clone(),
                    endpoint.clone(),
                )
            }
        });
        tasks.add(order_supervisor.run_log_summary());
        let (collab_settlement_supervisor, collab_settlement_addr) = Supervisor::new({
            let endpoint_addr = endpoint_addr.clone();
            let executor = executor.clone();
            move || {
                collab_settlement::taker::Actor::new(
                    endpoint_addr.clone(),
                    executor.clone(),
                    n_payouts,
                )
            }
        });
        tasks.add(collab_settlement_supervisor.run_log_summary());

        let cfd_actor_addr = taker_cfd::Actor::new(
            db.clone(),
            projection_actor,
            collab_settlement_addr,
            order,
            maker_identity,
            PeerId::from(
                maker_multiaddr
                    .clone()
                    .extract_peer_id()
                    .context("Unable to extract peer id from maker address")?,
            ),
        )
        .create(None)
        .spawn(&mut tasks);

        let (rollover_supervisor, rollover_addr) = Supervisor::new({
            let endpoint_addr = endpoint_addr.clone();
            let executor = executor.clone();
            let oracle_addr = oracle_addr.clone();
            move || {
                rollover::taker::Actor::new(
                    endpoint_addr.clone(),
                    executor.clone(),
                    oracle_pk,
                    oracle::AnnouncementsChannel::new(oracle_addr.clone().into()),
                    n_payouts,
                )
            }
        });
        tasks.add(rollover_supervisor.run_log_summary());

        let auto_rollover_addr = auto_rollover::Actor::new(db.clone(), rollover_addr)
            .create(None)
            .spawn(&mut tasks);

        let online_status_actor = online_status::Actor::new(
            endpoint_addr.clone(),
            maker_multiaddr
                .clone()
                .extract_peer_id()
                .expect("to be able to extract peer id"),
            maker_online_status_feed_sender,
        )
        .create(None)
        .spawn(&mut tasks);

        tasks.add(monitor_ctx.run(monitor_constructor(executor.clone())?));
        tasks.add(oracle_ctx.run(oracle_constructor(executor.clone())));

        let dialer_constructor = {
            let endpoint_addr = endpoint_addr.clone();
            move || dialer::Actor::new(endpoint_addr.clone(), maker_multiaddr.clone())
        };
        let (dialer_supervisor, dialer_actor) = Supervisor::<_, dialer::Error>::with_policy(
            dialer_constructor,
            always_restart_after(RESTART_INTERVAL),
        );

        let (offer_supervisor, offer_addr) = Supervisor::new({
            let cfd_actor_addr = cfd_actor_addr.clone();
            move || offer::taker::Actor::new(cfd_actor_addr.clone().into())
        });

        let (identify_listener_supervisor, identify_listener_actor) = Supervisor::new({
            let identity = identity.libp2p.clone();
            move || {
                identify::listener::Actor::new(
                    version(),
                    environment.clone(),
                    identity.public(),
                    HashSet::new(),
                    TAKER_LISTEN_PROTOCOLS.into(),
                )
            }
        });

        let (identify_dialer_actor, identify_info_feed_receiver) =
            identify::dialer::Actor::new_with_subscriber(endpoint_addr.clone());
        let identify_dialer_actor = identify_dialer_actor.create(None).spawn(&mut tasks);

        let pong_address = pong::Actor.create(None).spawn(&mut tasks);

        let (supervisor, ping_actor) =
            Supervisor::new(move || ping::Actor::new(endpoint_addr.clone(), PING_INTERVAL));
        tasks.add(supervisor.run_log_summary());

        let endpoint = Endpoint::new(
            Box::new(TokioTcpConfig::new),
            identity.libp2p,
            ENDPOINT_CONNECTION_TIMEOUT,
            TAKER_LISTEN_PROTOCOLS.inbound_substream_handlers(
                pong_address.clone(),
                identify_listener_actor,
                offer_addr,
            ),
            endpoint::Subscribers::new(
                vec![
                    online_status_actor.clone().into(),
                    ping_actor.clone().into(),
                    identify_dialer_actor.clone().into(),
                ],
                vec![
                    dialer_actor.into(),
                    ping_actor.into(),
                    online_status_actor.clone().into(),
                    identify_dialer_actor.clone().into(),
                ],
                vec![],
                vec![],
            ),
            Arc::new(HashSet::default()), // Taker does not block peers
        );

        tasks.add(endpoint_context.run(endpoint));

        tasks.add(dialer_supervisor.run_log_summary());
        tasks.add(offer_supervisor.run_log_summary());
        tasks.add(identify_listener_supervisor.run_log_summary());

        let close_cfds_actor = archive_closed_cfds::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);
        let archive_failed_cfds_actor = archive_failed_cfds::Actor::new(db)
            .create(None)
            .spawn(&mut tasks);

        tracing::debug!("Taker actor system ready");

        Ok(Self {
            cfd_actor: cfd_actor_addr,
            wallet_actor: wallet_actor_addr,
            _oracle_actor: oracle_addr,
            auto_rollover_actor: auto_rollover_addr,
            price_feed_actor,
            executor,
            _close_cfds_actor: close_cfds_actor,
            _archive_failed_cfds_actor: archive_failed_cfds_actor,
            _tasks: tasks,
            maker_online_status_feed_receiver,
            identify_info_feed_receiver,
            _online_status_actor: online_status_actor,
            _pong_actor: pong_address,
            _identify_dialer_actor: identify_dialer_actor,
        })
    }

    #[instrument(skip(self), err)]
    pub async fn place_order(
        &self,
        offer_id: OfferId,
        quantity: Contracts,
        leverage: Leverage,
    ) -> Result<OrderId> {
        let order_id = self
            .cfd_actor
            .send(taker_cfd::PlaceOrder {
                offer_id,
                quantity,
                leverage,
            })
            .await??;

        Ok(order_id)
    }

    #[instrument(skip(self), err)]
    pub async fn commit(&self, order_id: OrderId) -> Result<()> {
        self.executor
            .execute(order_id, |cfd| cfd.manual_commit_to_blockchain())
            .await?;

        Ok(())
    }

    #[instrument(skip(self), err)]
    pub async fn propose_settlement(&self, order_id: OrderId) -> Result<()> {
        let contract_symbol = self
            .executor
            .query(order_id, |cfd| Ok(cfd.contract_symbol()))
            .await?;

        let latest_quote = *self
            .price_feed_actor
            .send(xtra_bitmex_price_feed::GetLatestQuotes)
            .await
            .context("Price feed not available")?
            .get(&into_price_feed_symbol(contract_symbol))
            .context("No quote available")?;

        let quote_timestamp = latest_quote
            .timestamp
            .format(&time::format_description::well_known::Rfc3339)
            .context("Failed to format timestamp")?;

        let threshold = QUOTE_INTERVAL_MINUTES.minutes() * 2;

        if latest_quote.is_older_than(threshold) {
            bail!(
                "Latest quote is older than {} minutes. Refusing to settle with old price.",
                threshold.whole_minutes()
            )
        }

        self.cfd_actor
            .send(taker_cfd::ProposeSettlement {
                order_id,
                bid: Price::new(latest_quote.bid())?,
                ask: Price::new(latest_quote.ask())?,
                quote_timestamp,
            })
            .await?
    }

    #[instrument(skip(self), err)]
    pub async fn withdraw(
        &self,
        amount: Option<Amount>,
        address: bitcoin::Address,
        fee_rate: FeeRate,
    ) -> Result<Txid> {
        self.wallet_actor
            .send(wallet::Withdraw {
                amount,
                address,
                fee: Some(fee_rate),
            })
            .await?
    }

    #[instrument(skip(self), err)]
    pub async fn sync_wallet(&self) -> Result<()> {
        self.wallet_actor.send(wallet::Sync).await?;
        Ok(())
    }
}

/// A struct defining our environment
///
/// We can run on all kinds of environment, hence this is just a wrapper around string.
/// However, for backwards compatibility with <=0.6.x we need to support to support
/// `Unknown`. For all other we format the string to lowercase.
#[derive(Debug, Clone, Display, PartialEq, Eq)]
pub struct Environment(String);

impl Environment {
    pub fn new(val: &str) -> Environment {
        Self(Environment::parse_known_variances(val))
    }

    pub fn unknown() -> Environment {
        Self("Unknown".to_string())
    }

    pub fn as_string(&self) -> String {
        self.0.clone()
    }

    fn parse_known_variances(string: &str) -> String {
        match string.to_lowercase().as_str() {
            "unknown" => "Unknown".to_string(),
            s => s.to_string(),
        }
    }
}

/// The version of the `daemon` crate, as specified in its `Cargo.toml` file.
pub fn version() -> String {
    VERSION.to_string()
}

fn into_price_feed_symbol(symbol: model::ContractSymbol) -> xtra_bitmex_price_feed::ContractSymbol {
    match symbol {
        model::ContractSymbol::BtcUsd => xtra_bitmex_price_feed::ContractSymbol::BtcUsd,
        model::ContractSymbol::EthUsd => xtra_bitmex_price_feed::ContractSymbol::EthUsd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_test_environment_from_str_or_unknown() {
        assert_eq!(Environment::new("umbrel").as_string(), "umbrel".to_string());
        assert_eq!(
            Environment::new("unknown").as_string(),
            "Unknown".to_string()
        );
    }
}
