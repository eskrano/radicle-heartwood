use std::{io, net};

use crossbeam_channel as chan;
use nakamoto_net::{LocalTime, Reactor};
use thiserror::Error;

use crate::clock::RefClock;
use crate::profile::Profile;
use crate::service::routing;
use crate::transport::Transport;
use crate::wire::Wire;
use crate::{address, service};

pub mod handle;

/// Directory in `$RAD_HOME` under which node-specific files are stored.
pub const NODE_DIR: &str = "node";
/// Filename of routing table database under [`NODE_DIR`].
pub const ROUTING_DB_FILE: &str = "routing.db";
/// Filename of address database under [`NODE_DIR`].
pub const ADDRESS_DB_FILE: &str = "addresses.db";

/// A client error.
#[derive(Error, Debug)]
pub enum Error {
    /// A routing database error.
    #[error("routing database error: {0}")]
    Routing(#[from] routing::Error),
    /// An address database error.
    #[error("address database error: {0}")]
    Addresses(#[from] address::Error),
    /// An I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// A networking error.
    #[error("network error: {0}")]
    Net(#[from] nakamoto_net::error::Error),
}

/// Client configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Client service configuration.
    pub service: service::Config,
    /// Client listen addresses.
    pub listen: Vec<net::SocketAddr>,
}

impl Config {
    /// Create a new configuration for the given network.
    pub fn new(network: service::Network) -> Self {
        Self {
            service: service::Config {
                network,
                ..service::Config::default()
            },
            ..Self::default()
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            service: service::Config::default(),
            listen: vec![([0, 0, 0, 0], 0).into()],
        }
    }
}

pub struct Client<R: Reactor> {
    reactor: R,
    profile: Profile,

    handle: chan::Sender<service::Command>,
    commands: chan::Receiver<service::Command>,
    shutdown: chan::Sender<()>,
    listening: chan::Receiver<net::SocketAddr>,
    events: Events,
}

impl<R: Reactor> Client<R> {
    pub fn new(profile: Profile) -> Result<Self, Error> {
        let (handle, commands) = chan::unbounded::<service::Command>();
        let (shutdown, shutdown_recv) = chan::bounded(1);
        let (listening_send, listening) = chan::bounded(1);
        let reactor = R::new(shutdown_recv, listening_send)?;
        let events = Events {};

        Ok(Self {
            profile,
            reactor,
            handle,
            commands,
            listening,
            shutdown,
            events,
        })
    }

    pub fn run(mut self, config: Config) -> Result<(), Error> {
        let network = config.service.network;
        let rng = fastrand::Rng::new();
        let time = LocalTime::now();
        let storage = self.profile.storage;
        let signer = self.profile.signer;
        let addresses =
            address::Book::open(self.profile.home.join(NODE_DIR).join(ADDRESS_DB_FILE))?;
        let routing = routing::Table::open(self.profile.home.join(NODE_DIR).join(ROUTING_DB_FILE))?;

        log::info!("Initializing client ({:?})..", network);

        let service = service::Service::new(
            config.service,
            RefClock::from(time),
            routing,
            storage,
            addresses,
            signer,
            rng,
        );

        self.reactor.run(
            &config.listen,
            Transport::new(Wire::new(service)),
            self.events,
            self.commands,
        )?;

        Ok(())
    }

    /// Create a new handle to communicate with the client.
    pub fn handle(&self) -> handle::Handle<R::Waker> {
        handle::Handle {
            waker: self.reactor.waker(),
            commands: self.handle.clone(),
            shutdown: self.shutdown.clone(),
            listening: self.listening.clone(),
        }
    }
}

pub struct Events {}

impl nakamoto_net::Publisher<service::Event> for Events {
    fn publish(&mut self, e: service::Event) {
        log::info!("Received event {:?}", e);
    }
}
