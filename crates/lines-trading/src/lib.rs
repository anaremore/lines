//! The trading core, headless (roadmap 6, decisions 0004, 0005, 0006, 0012, 0018).
//!
//! **Commands in, events out.** Every user action that trades is a typed [`Command`] with an
//! account and a contract; [`TradingCore`] (pure, deterministic) runs it through validation,
//! prop rules ([`rules`]) and pre-trade risk ([`risk`]) to a [`Broker`], and reports everything
//! as an ordered stream of [`Event`]s: command results, order updates, fills, positions,
//! account and rule status. [`TradingService`] runs the core on its own thread; the UI holds a
//! [`TradingHandle`] (submit commands, subscribe to events) and never a lock.
//!
//! - [`order`]: the one order lifecycle.
//! - [`broker`]: the `Broker` trait with capabilities; [`paper`]: `PaperBroker` (fill models,
//!   book walking, stops, partial fills, fees, sessions, server-side brackets/OCO).
//! - [`position`]: exact FIFO P&L; [`types::Money`]: exact money.
//! - [`persist`]: the versioned paper trading file.
//! - [`extension`]: the broker extension point (decision 0020): brokers registered from outside
//!   the public crates, with capabilities and an account source.
//!
//! No GPUI. Live brokers come later through decision 0014's private connector.

pub mod broker;
pub mod command;
pub mod core;
pub mod event;
pub mod extension;
pub mod market;
pub mod order;
pub mod paper;
pub mod persist;
pub mod position;
pub mod risk;
pub mod rules;
pub mod service;
pub mod types;

pub use broker::{Broker, BrokerCaps, BrokerEvent, BrokerRequest, BrokerSnapshot};
pub use command::{BracketSpec, Command, OrderSpec, Reject, RejectCode, Source};
pub use core::{AccountConfig, TradingCore};
pub use event::{AccountView, Event, FillEvent, PositionView};
pub use market::MarketInput;
pub use order::{Order, OrderState, Role};
pub use paper::{FillModel, PaperBroker, PaperConfig};
pub use risk::RiskLimits;
pub use rules::{RulePreset, RuleStatus, RuleTracker};
pub use service::{Clock, FeedBridge, TradingHandle, TradingService};
pub use types::{AccountId, CommandId, Money, OrderId, OrderKind, Side};
