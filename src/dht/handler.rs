use addr::AsV4Address;
use dht::Dht;
use errors::{Error, ErrorKind, Result};
use proto::{Addr, Message, MessageType, NodeID, Query, Response};
use routing::{FindNodeResult, RoutingTable};
use stream::run_forever;
use transport::Request;

use std::{
    net::{SocketAddr, SocketAddrV4},
    ops::DerefMut,
};

use tokio::prelude::*;

impl Dht {
    pub fn handle_requests<S: Stream<Item = (Request, SocketAddr), Error = Error>>(
        self,
        stream: S,
    ) -> impl Future<Item = (), Error = ()> {
        run_forever(
            stream
                .and_then(move |(request, from)| {
                    let response = self.handle_request(request, from.into_v4()?);
                    self.send_transport.send(from, response)
                }).or_else(|err| {
                    eprintln!("Error While Handling Requests: {}", err);

                    Ok(())
                }),
        )
    }

    fn handle_request(&self, request: Request, from: SocketAddrV4) -> Message {
        let result = match request.query {
            Query::Ping { id } => self.handle_ping(from, id, request.read_only),
            Query::FindNode { id, target } => {
                self.handle_find_node(from, id, target, request.read_only)
            }
            Query::GetPeers { id, info_hash } => {
                self.handle_get_peers(from, id, info_hash, request.read_only)
            }
            Query::AnnouncePeer {
                id,
                implied_port,
                port,
                info_hash,
                token,
            } => self.handle_announce_peer(
                from,
                id,
                implied_port,
                port,
                info_hash,
                token,
                request.read_only,
            ),
            _ => Err(ErrorKind::UnimplementedRequestType.into()),
        };

        let message_type = match result {
            Ok(response) => MessageType::Response { response },
            Err(err) => MessageType::Error {
                error: err.as_request_error(),
            },
        };

        Message {
            ip: None,
            transaction_id: request.transaction_id,
            version: None,
            message_type,
            read_only: false,
        }
    }

    fn handle_ping(&self, from: SocketAddrV4, id: NodeID, read_only: bool) -> Result<Response> {
        let mut routing_table = self.routing_table.lock()?;
        record_request(&mut routing_table, id, from, read_only)?;

        Ok(Response::OnlyId {
            id: self.id.clone(),
        })
    }

    fn handle_find_node(
        &self,
        from: SocketAddrV4,
        id: NodeID,
        target: NodeID,
        read_only: bool,
    ) -> Result<Response> {
        let mut routing_table = self.routing_table.lock()?;
        record_request(&mut routing_table, id, from, read_only)?;

        let nodes = match routing_table.find_node(&target) {
            FindNodeResult::Node(node) => vec![node],
            FindNodeResult::Nodes(nodes) => nodes,
        };

        Ok(Response::NextHop {
            id: self.id.clone(),
            token: None,
            nodes,
        })
    }

    fn handle_get_peers(
        &self,
        from: SocketAddrV4,
        id: NodeID,
        info_hash: NodeID,
        read_only: bool,
    ) -> Result<Response> {
        let mut routing_table = self.routing_table.lock()?;
        record_request(&mut routing_table, id, from, read_only)?;

        let token_bytes = routing_table.generate_token(&from).to_vec();
        let token = Some(token_bytes);
        let torrents = self.torrents.lock()?;
        let torrent = torrents.get(&info_hash);

        if let Some(peers) = torrent {
            Ok(Response::GetPeers {
                id: self.id.clone(),
                token,
                peers: peers.iter().map(|peer| Addr::from(peer.clone())).collect(),
            })
        } else {
            let nodes = routing_table.find_nodes(&info_hash);

            Ok(Response::NextHop {
                id: self.id.clone(),
                token,
                nodes,
            })
        }
    }

    fn handle_announce_peer(
        &self,
        mut from: SocketAddrV4,
        id: NodeID,
        implied_port: bool,
        port: Option<u16>,
        info_hash: NodeID,
        token: Vec<u8>,
        read_only: bool,
    ) -> Result<Response> {
        let mut routing_table = self.routing_table.lock()?;

        if !routing_table.verify_token(&token, &from) {
            return Err(ErrorKind::InvalidToken)?;
        };

        let addr = if implied_port {
            from
        } else {
            let actual_port = match port {
                None => return Err(ErrorKind::InsufficientAddress)?,
                Some(port) => port,
            };

            from.set_port(actual_port);
            from
        };

        record_request(&mut routing_table, id, from, read_only)?;

        let mut torrents = self.torrents.lock()?;

        torrents
            .entry(info_hash)
            .or_insert_with(Vec::new)
            .push(addr);

        Ok(Response::OnlyId {
            id: self.id.clone(),
        })
    }
}

fn record_request<T: DerefMut<Target = RoutingTable>>(
    routing_table: &mut T,
    id: NodeID,
    from: SocketAddrV4,
    read_only: bool,
) -> Result<()> {
    if !read_only {
        routing_table
            .deref_mut()
            .get_or_add(id, from)
            .map(|node| node.mark_successful_request_from());
    }

    Ok(())
}
