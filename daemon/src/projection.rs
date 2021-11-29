use std::collections::HashMap;

use crate::model::cfd::{OrderId, Role, SettlementKind, UpdateCfdProposal};
use crate::model::{Leverage, Position, Timestamp, TradingPair};
use crate::{bitmex_price_feed, model, tx, Cfd, Order, UpdateCfdProposals};
use bdk::bitcoin::{Amount, Network};
use itertools::Itertools;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use xtra_productivity::xtra_productivity;

pub struct Actor {
    tx: Tx,
}

pub struct Feeds {
    pub quote: watch::Receiver<Quote>,
    pub order: watch::Receiver<Option<CfdOrder>>,
    pub connected_takers: watch::Receiver<Vec<Identity>>,
    // TODO: Convert items below here into projections
    pub cfds: watch::Receiver<Vec<Cfd>>,
    pub settlements: watch::Receiver<UpdateCfdProposals>,
}

impl Actor {
    pub fn new(init_cfds: Vec<Cfd>, init_quote: bitmex_price_feed::Quote) -> (Self, Feeds) {
        let (tx_cfds, rx_cfds) = watch::channel(init_cfds);
        let (tx_order, rx_order) = watch::channel(None);
        let (tx_update_cfd_feed, rx_update_cfd_feed) = watch::channel(HashMap::new());
        let (tx_quote, rx_quote) = watch::channel(init_quote.into());
        let (tx_connected_takers, rx_connected_takers) = watch::channel(Vec::new());

        (
            Self {
                tx: Tx {
                    cfds: tx_cfds,
                    order: tx_order,
                    quote: tx_quote,
                    settlements: tx_update_cfd_feed,
                    connected_takers: tx_connected_takers,
                },
            },
            Feeds {
                cfds: rx_cfds,
                order: rx_order,
                quote: rx_quote,
                settlements: rx_update_cfd_feed,
                connected_takers: rx_connected_takers,
            },
        )
    }
}

/// Internal struct to keep all the senders around in one place
struct Tx {
    pub cfds: watch::Sender<Vec<Cfd>>,
    pub order: watch::Sender<Option<CfdOrder>>,
    pub quote: watch::Sender<Quote>,
    pub settlements: watch::Sender<UpdateCfdProposals>,
    // TODO: Use this channel to communicate maker status as well with generic
    // ID of connected counterparties
    pub connected_takers: watch::Sender<Vec<Identity>>,
}

pub struct Update<T>(pub T);

#[xtra_productivity]
impl Actor {
    fn handle(&mut self, msg: Update<Option<Order>>) {
        let _ = self.tx.order.send(msg.0.map(|x| x.into()));
    }
    fn handle(&mut self, msg: Update<bitmex_price_feed::Quote>) {
        let _ = self.tx.quote.send(msg.0.into());
    }
    fn handle(&mut self, msg: Update<Vec<Cfd>>) {
        let _ = self.tx.cfds.send(msg.0);
    }
    fn handle(&mut self, msg: Update<UpdateCfdProposals>) {
        let _ = self.tx.settlements.send(msg.0);
    }
    fn handle(&mut self, msg: Update<Vec<model::Identity>>) {
        let _ = self
            .tx
            .connected_takers
            .send(msg.0.iter().map(|x| x.into()).collect_vec());
    }
}

impl xtra::Actor for Actor {}

/// Types

#[derive(Debug, Clone, PartialEq)]
pub struct Usd {
    inner: model::Usd,
}

impl Usd {
    fn new(usd: model::Usd) -> Self {
        Self {
            inner: model::Usd::new(usd.into_decimal().round_dp(2)),
        }
    }
}

impl From<model::Usd> for Usd {
    fn from(usd: model::Usd) -> Self {
        Self::new(usd)
    }
}

impl Serialize for Usd {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        <Decimal as Serialize>::serialize(&self.inner.into_decimal(), serializer)
    }
}

impl Serialize for Price {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        <Decimal as Serialize>::serialize(&self.inner.into_decimal(), serializer)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    inner: model::Price,
}

impl Price {
    fn new(price: model::Price) -> Self {
        Self {
            inner: model::Price::new(price.into_decimal().round_dp(2)).expect(
                "rounding a valid price to 2 decimal places should still result in a valid price",
            ),
        }
    }
}

impl From<model::Price> for Price {
    fn from(price: model::Price) -> Self {
        Self::new(price)
    }
}

// TODO: Remove this after CfdsWithAuxData is removed
impl From<Price> for model::Price {
    fn from(price: Price) -> Self {
        price.inner
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    bid: Price,
    ask: Price,
    last_updated_at: Timestamp,
}

impl From<bitmex_price_feed::Quote> for Quote {
    fn from(quote: bitmex_price_feed::Quote) -> Self {
        Quote {
            bid: quote.bid.into(),
            ask: quote.ask.into(),
            last_updated_at: quote.timestamp,
        }
    }
}

// TODO: Remove this after CfdsWithAuxData is removed
impl From<Quote> for bitmex_price_feed::Quote {
    fn from(quote: Quote) -> Self {
        Self {
            timestamp: quote.last_updated_at,
            bid: quote.bid.into(),
            ask: quote.ask.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rust_decimal_macros::dec;
    use serde_test::{assert_ser_tokens, Token};

    #[test]
    fn usd_serializes_with_only_cents() {
        let usd = Usd::new(model::Usd::new(dec!(1000.12345)));

        assert_ser_tokens(&usd, &[Token::Str("1000.12")]);
    }

    #[test]
    fn price_serializes_with_only_cents() {
        let price = Price::new(model::Price::new(dec!(1000.12345)).unwrap());

        assert_ser_tokens(&price, &[Token::Str("1000.12")]);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CfdOrder {
    pub id: OrderId,

    pub trading_pair: TradingPair,
    pub position: Position,

    pub price: Price,

    pub min_quantity: Usd,
    pub max_quantity: Usd,

    pub leverage: Leverage,
    pub liquidation_price: Price,

    pub creation_timestamp: Timestamp,
    pub settlement_time_interval_in_secs: u64,
}

impl From<Order> for CfdOrder {
    fn from(order: Order) -> Self {
        Self {
            id: order.id,
            trading_pair: order.trading_pair,
            position: order.position,
            price: order.price.into(),
            min_quantity: order.min_quantity.into(),
            max_quantity: order.max_quantity.into(),
            leverage: order.leverage,
            liquidation_price: order.liquidation_price.into(),
            creation_timestamp: order.creation_timestamp,
            settlement_time_interval_in_secs: order
                .settlement_interval
                .whole_seconds()
                .try_into()
                .expect("settlement_time_interval_hours is always positive number"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, derive_more::Display)]
pub struct Identity(String);

impl From<&model::Identity> for Identity {
    fn from(id: &model::Identity) -> Self {
        Self(id.to_string())
    }
}

impl From<model::Identity> for Identity {
    fn from(id: model::Identity) -> Self {
        Self(id.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum CfdState {
    OutgoingOrderRequest,
    IncomingOrderRequest,
    Accepted,
    Rejected,
    ContractSetup,
    PendingOpen,
    Open,
    PendingCommit,
    PendingCet,
    PendingClose,
    OpenCommitted,
    IncomingSettlementProposal,
    OutgoingSettlementProposal,
    IncomingRollOverProposal,
    OutgoingRollOverProposal,
    Closed,
    PendingRefund,
    Refunded,
    SetupFailed,
}

pub fn to_cfd_state(
    cfd_state: &model::cfd::CfdState,
    proposal_status: Option<&UpdateCfdProposal>,
) -> CfdState {
    match proposal_status {
        Some(UpdateCfdProposal::Settlement {
            direction: SettlementKind::Outgoing,
            ..
        }) => CfdState::OutgoingSettlementProposal,
        Some(UpdateCfdProposal::Settlement {
            direction: SettlementKind::Incoming,
            ..
        }) => CfdState::IncomingSettlementProposal,
        Some(UpdateCfdProposal::RollOverProposal {
            direction: SettlementKind::Outgoing,
            ..
        }) => CfdState::OutgoingRollOverProposal,
        Some(UpdateCfdProposal::RollOverProposal {
            direction: SettlementKind::Incoming,
            ..
        }) => CfdState::IncomingRollOverProposal,
        None => match cfd_state {
            // Filled in collaborative close in Open means that we're awaiting
            // a collaborative closure
            model::cfd::CfdState::Open {
                collaborative_close: Some(_),
                ..
            } => CfdState::PendingClose,
            model::cfd::CfdState::OutgoingOrderRequest { .. } => CfdState::OutgoingOrderRequest,
            model::cfd::CfdState::IncomingOrderRequest { .. } => CfdState::IncomingOrderRequest,
            model::cfd::CfdState::Accepted { .. } => CfdState::Accepted,
            model::cfd::CfdState::Rejected { .. } => CfdState::Rejected,
            model::cfd::CfdState::ContractSetup { .. } => CfdState::ContractSetup,
            model::cfd::CfdState::PendingOpen { .. } => CfdState::PendingOpen,
            model::cfd::CfdState::Open { .. } => CfdState::Open,
            model::cfd::CfdState::OpenCommitted { .. } => CfdState::OpenCommitted,
            model::cfd::CfdState::PendingRefund { .. } => CfdState::PendingRefund,
            model::cfd::CfdState::Refunded { .. } => CfdState::Refunded,
            model::cfd::CfdState::SetupFailed { .. } => CfdState::SetupFailed,
            model::cfd::CfdState::PendingCommit { .. } => CfdState::PendingCommit,
            model::cfd::CfdState::PendingCet { .. } => CfdState::PendingCet,
            model::cfd::CfdState::Closed { .. } => CfdState::Closed,
        },
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CfdDetails {
    tx_url_list: Vec<tx::TxUrl>,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc::opt")]
    payout: Option<Amount>,
}

// TODO: don't expose
pub fn to_cfd_details(cfd: &model::cfd::Cfd, network: Network) -> CfdDetails {
    CfdDetails {
        tx_url_list: tx::to_tx_url_list(cfd.state.clone(), network),
        payout: cfd.payout(),
    }
}

#[derive(Debug, derive_more::Display, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum CfdAction {
    AcceptOrder,
    RejectOrder,
    Commit,
    Settle,
    AcceptSettlement,
    RejectSettlement,
    RollOver,
    AcceptRollOver,
    RejectRollOver,
}

pub fn available_actions(state: CfdState, role: Role) -> Vec<CfdAction> {
    match (state, role) {
        (CfdState::IncomingOrderRequest { .. }, Role::Maker) => {
            vec![CfdAction::AcceptOrder, CfdAction::RejectOrder]
        }
        (CfdState::IncomingSettlementProposal { .. }, Role::Maker) => {
            vec![CfdAction::AcceptSettlement, CfdAction::RejectSettlement]
        }
        (CfdState::IncomingRollOverProposal { .. }, Role::Maker) => {
            vec![CfdAction::AcceptRollOver, CfdAction::RejectRollOver]
        }
        // If there is an outgoing settlement proposal already, user can't
        // initiate new one
        (CfdState::OutgoingSettlementProposal { .. }, Role::Maker) => {
            vec![CfdAction::Commit]
        }
        // User is awaiting collaborative close, commit is left as a safeguard
        (CfdState::PendingClose { .. }, _) => {
            vec![CfdAction::Commit]
        }
        (CfdState::Open { .. }, Role::Taker) => {
            vec![CfdAction::RollOver, CfdAction::Commit, CfdAction::Settle]
        }
        (CfdState::Open { .. }, Role::Maker) => vec![CfdAction::Commit],
        _ => vec![],
    }
}
