// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

use crate::config::OurType;
use crate::connection::{BootstrapGroupMaker, Connection, FromPeer, QConn, ToPeer};
use crate::context::ctx_mut;
use crate::error::Error;
use crate::event::Event;
use crate::peer_config;
use crate::utils;
use crate::wire_msg::{Handshake, WireMsg};
use crate::{communicate, NodeInfo, Peer, R};
use std::mem;
use std::net::SocketAddr;
use tokio::prelude::{Future, Stream};
use tokio::runtime::current_thread;

/// Connect to the given peer
pub fn connect_to(
    peer_info: NodeInfo,
    send_after_connect: Option<WireMsg>,
    bootstrap_group_maker: Option<&BootstrapGroupMaker>,
) -> R<()> {
    let peer_addr = peer_info.peer_addr;

    let peer_cfg = match peer_config::new_client_cfg(&peer_info.peer_cert_der) {
        Ok(cfg) => cfg,
        Err(e) => {
            handle_connect_err(peer_addr, &e);
            return Err(e);
        }
    };

    let r = ctx_mut(|c| {
        let event_tx = c.event_tx.clone();

        let (terminator, rx) = utils::connect_terminator();

        let conn = c.connections.entry(peer_addr).or_insert_with(|| {
            Connection::new(
                peer_addr,
                event_tx,
                bootstrap_group_maker
                    .map(|m| m.add_member_and_get_group_ref(peer_addr, terminator.clone())),
            )
        });

        if conn.to_peer.is_no_connection() {
            // TODO see if this can be the default from-peer for OurType::Client
            if c.our_type == OurType::Client {
                if !conn.from_peer.is_no_connection() {
                    panic!("Logic Error - cannot expect Network to reverse connect to a client");
                }
                conn.from_peer = FromPeer::NotNeeded;
            }

            // If we already had an incoming from someone we are trying to bootstrap off
            if conn.bootstrap_group_ref.is_none() {
                conn.bootstrap_group_ref = bootstrap_group_maker
                    .map(|b| b.add_member_and_get_group_ref(peer_addr, terminator.clone()));
            }

            let mut pending_sends: Vec<_> = Default::default();
            if let Some(pending_send) = send_after_connect {
                pending_sends.push(pending_send);
            }
            conn.to_peer = ToPeer::Initiated {
                terminator: terminator.clone(),
                peer_cert_der: peer_info.peer_cert_der,
                pending_sends,
            };
            c.quic_ep()
                .connect_with(peer_cfg, &peer_addr, "MaidSAFE.net")
                .map_err(Error::from)
                .and_then(move |new_client_conn_fut| {
                    let terminator_leaf = rx
                        .map_err(move |_| {
                            handle_connect_err(peer_addr, &Error::ConnectionCancelled)
                        })
                        .for_each(move |_| {
                            handle_connect_err(peer_addr, &Error::ConnectionCancelled);
                            Err(())
                        });
                    let handle_new_connection_res_leaf =
                        new_client_conn_fut.then(move |new_peer_conn_res| {
                            handle_new_connection_res(peer_addr, new_peer_conn_res);
                            Ok::<_, ()>(())
                        });
                    let leaf = terminator_leaf
                        .select(handle_new_connection_res_leaf)
                        .then(|_| Ok(()));

                    current_thread::spawn(leaf);

                    Ok(())
                })
        } else {
            Err(Error::DuplicateConnectionToPeer(peer_addr))
        }
    });

    if let Err(e) = r.as_ref() {
        handle_connect_err(peer_addr, e);
    }

    r
}

fn handle_new_connection_res(
    peer_addr: SocketAddr,
    new_peer_conn_res: Result<
        (
            quinn::ConnectionDriver,
            quinn::Connection,
            quinn::IncomingStreams,
        ),
        quinn::ConnectionError,
    >,
) {
    let (conn_driver, q_conn, incoming_streams) = match new_peer_conn_res {
        Ok((conn_driver, q_conn, incoming_streams)) => {
            (conn_driver, QConn::from(q_conn), incoming_streams)
        }
        Err(e) => return handle_connect_err(peer_addr, &From::from(e)),
    };
    current_thread::spawn(
        conn_driver.map_err(move |e| handle_connect_err(peer_addr, &From::from(e))),
    );

    trace!("Successfully connected to peer: {}", peer_addr);

    let mut should_accept_incoming = false;

    ctx_mut(|c| {
        let conn = match c.connections.get_mut(&peer_addr) {
            Some(conn) => conn,
            None => {
                trace!(
                    "Made a successful connection to a peer we are no longer interested in or \
                     the peer had its connection to us servered which made us forget this peer: {}",
                    peer_addr
                );
                return;
            }
        };

        let mut to_peer_prev = mem::replace(&mut conn.to_peer, Default::default());
        let (peer_cert_der, pending_sends) = match to_peer_prev {
            ToPeer::Initiated {
                ref mut peer_cert_der,
                ref mut pending_sends,
                ..
            } => (
                mem::replace(peer_cert_der, Default::default()),
                mem::replace(pending_sends, Default::default()),
            ),
            // TODO analyse if this is actually reachable in some wierd case where things were in
            // the event loop and resolving now etc
            x => unreachable!(
                "TODO We can handle new connection only because it was previously \
                 initiated: {:?}",
                x
            ),
        };

        let node_info = NodeInfo {
            peer_addr,
            peer_cert_der: peer_cert_der.clone(),
        };
        if conn.we_contacted_peer {
            c.bootstrap_cache.add_peer(node_info.clone());
        }

        match conn.from_peer {
            FromPeer::NoConnection => {
                communicate::write_to_peer_connection(
                    peer_addr,
                    &q_conn,
                    WireMsg::Handshake(Handshake::Node {
                        cert_der: c.our_complete_cert.cert_der.clone(),
                    }),
                );
            }
            FromPeer::NotNeeded => {
                communicate::write_to_peer_connection(
                    peer_addr,
                    &q_conn,
                    WireMsg::Handshake(Handshake::Client),
                );

                let event = if let Some(bootstrap_group_ref) = conn.bootstrap_group_ref.take() {
                    bootstrap_group_ref.terminate_group(true);
                    Event::BootstrappedTo { node: node_info }
                } else {
                    Event::ConnectedTo {
                        peer: node_info.into(),
                    }
                };

                if let Err(e) = c.event_tx.send(event) {
                    info!("Could not fire event: {:?}", e);
                }

                should_accept_incoming = true;
            }
            FromPeer::Established {
                ref mut pending_reads,
                ..
            } => {
                let event = if let Some(bootstrap_group_ref) = conn.bootstrap_group_ref.take() {
                    bootstrap_group_ref.terminate_group(true);
                    Event::BootstrappedTo {
                        node: node_info.clone(),
                    }
                } else {
                    Event::ConnectedTo {
                        peer: node_info.clone().into(),
                    }
                };

                if let Err(e) = c.event_tx.send(event) {
                    info!("Could not fire event: {:?}", e);
                }

                let peer = Peer::Node { node_info };

                for pending_read in pending_reads.drain(..) {
                    communicate::dispatch_wire_msg(
                        peer.clone(),
                        &q_conn,
                        c.our_ext_addr_tx.take(),
                        &c.event_tx,
                        pending_read,
                        &mut c.bootstrap_cache,
                        conn.we_contacted_peer,
                    );
                }
            }
        }

        for pending_send in pending_sends {
            communicate::write_to_peer_connection(peer_addr, &q_conn, pending_send);
        }

        conn.to_peer = ToPeer::Established {
            peer_cert_der,
            q_conn,
        };
    });

    if should_accept_incoming {
        communicate::read_from_peer(peer_addr, incoming_streams);
    }
}

fn handle_connect_err(peer_addr: SocketAddr, e: &Error) {
    debug!(
        "Error connecting to peer {}: {:?} - Details: {}",
        peer_addr, e, e
    );

    if let Error::DuplicateConnectionToPeer(_) = e {
        return;
    }

    ctx_mut(|c| {
        if let Some(conn) = c.connections.remove(&peer_addr) {
            if !conn.from_peer.is_no_connection() {
                info!(
                    "Peer {} has a connection to us but we couldn't connect to it. \
                     All connections to this peer will now be severed.",
                    peer_addr
                );
            }
        }
    })
}
