use errors::{Error, ErrorKind, Result};
use failure::ResultExt;

use proto::{NodeID, Query};

use rand;

use std;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio;
use tokio::prelude::*;
use tokio::reactor::Handle;

use peer::inbound::{InboundMessagesFuture, TransactionMap, TxState};
use peer::messages::{
    FindNodeResponse, GetPeersResponse, NodeIDResponse, PortType, Request, Response, TransactionId,
};
use peer::response::ResponseFuture;

pub struct Peer {
    id: NodeID,

    /// Socket used for sending messages
    send_socket: std::net::UdpSocket,

    /// Collection of in-flight transactions awaiting a response
    transactions: Arc<Mutex<TransactionMap>>,
}

impl Peer {
    pub fn new(bind_address: SocketAddr) -> Result<Peer> {
        let send_socket = std::net::UdpSocket::bind(&bind_address).context(ErrorKind::BindError)?;

        Ok(Peer {
            id: NodeID::random(),
            send_socket,
            transactions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn handle_responses(&self) -> Result<InboundMessagesFuture> {
        let raw_recv_socket = self.send_socket.try_clone().context(ErrorKind::BindError)?;
        let recv_socket = tokio::net::UdpSocket::from_std(raw_recv_socket, &Handle::default())
            .context(ErrorKind::BindError)?;

        Ok(InboundMessagesFuture::new(
            recv_socket,
            self.transactions.clone(),
        ))
    }

    pub fn request(
        &self,
        address: SocketAddr,
        request: Request,
    ) -> impl Future<Item = Response, Error = Error> {
        let transaction_future = self.wait_for_response(request.transaction_id);

        self.send_request(address, request)
            .into_future()
            .and_then(move |_| transaction_future)
            .and_then(|envelope| Response::from(envelope))
    }

    /// Synchronously sends a request to `address`.
    ///
    /// The sending is done synchronously because doing it asynchronously was cumbersome and didn't
    /// make anything faster. UDP sending rarely blocks.
    fn send_request(&self, address: SocketAddr, request: Request) -> Result<()> {
        let transaction_id = request.transaction_id;
        let encoded = request.encode()?;

        self.send_socket
            .send_to(&encoded, &address)
            .with_context(|_| ErrorKind::SendError { to: address })?;

        self.transactions
            .lock()
            .map_err(|_| ErrorKind::LockPoisoned)
            .with_context(|_| ErrorKind::SendError { to: address })?
            .insert(transaction_id, TxState::AwaitingResponse { task: None });

        Ok(())
    }

    fn wait_for_response(&self, transaction_id: TransactionId) -> ResponseFuture {
        ResponseFuture::new(transaction_id, self.transactions.clone())
    }

    fn get_transaction_id() -> TransactionId {
        rand::random::<TransactionId>()
    }

    fn build_request(query: Query) -> Request {
        Request {
            transaction_id: Self::get_transaction_id(),
            version: None,
            query,
        }
    }

    pub fn ping(&self, address: SocketAddr) -> impl Future<Item = NodeID, Error = Error> {
        self.request(
            address,
            Self::build_request(Query::Ping {
                id: self.id.clone(),
            }),
        ).and_then(NodeIDResponse::from_response)
    }

    pub fn find_node(
        &self,
        address: SocketAddr,
        target: NodeID,
    ) -> impl Future<Item = FindNodeResponse, Error = Error> {
        self.request(
            address,
            Self::build_request(Query::FindNode {
                id: self.id.clone(),
                target,
            }),
        ).and_then(FindNodeResponse::from_response)
    }

    pub fn get_peers(
        &self,
        address: SocketAddr,
        info_hash: NodeID,
    ) -> impl Future<Item = GetPeersResponse, Error = Error> {
        self.request(
            address,
            Self::build_request(Query::GetPeers {
                id: self.id.clone(),
                info_hash,
            }),
        ).and_then(GetPeersResponse::from_response)
    }

    pub fn announce_peer(
        &self,
        token: Vec<u8>,
        address: SocketAddr,
        info_hash: NodeID,
        port_type: PortType,
    ) -> impl Future<Item = NodeID, Error = Error> {
        let (port, implied_port) = match port_type {
            PortType::Implied => (None, 1),
            PortType::Port(port) => (Some(port), 0),
        };

        self.request(
            address,
            Self::build_request(Query::AnnouncePeer {
                id: self.id.clone(),
                token,
                info_hash,
                port,
                implied_port,
            }),
        ).and_then(NodeIDResponse::from_response)
    }
}
