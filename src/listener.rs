// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

use crate::communicate;
use crate::connection::{Connection, FromPeer, QConn, ToPeer};
use crate::context::ctx_mut;
use crate::event::Event;
use crate::utils;
use crate::NodeInfo;
use tokio::prelude::{Future, Stream};
use tokio::runtime::current_thread;

/// Start listening
pub fn listen(incoming_connections: quinn::Incoming) {
    let leaf = incoming_connections
        .map_err(|()| warn!("ERROR: Listener errored out"))
        .for_each(move |(conn_driver, q_conn, incoming)| {
            handle_new_conn(conn_driver, q_conn, incoming);
            Ok(())
        });

    current_thread::spawn(leaf);
}

fn handle_new_conn(
    conn_driver: quinn::ConnectionDriver,
    q_conn: quinn::Connection,
    incoming_streams: quinn::IncomingStreams,
) {
    let q_conn = QConn::from(q_conn);

    let peer_addr = q_conn.remote_address();

    current_thread::spawn(conn_driver.map_err(move |e| {
        utils::handle_communication_err(peer_addr, &From::from(e), "Driver failed");
    }));

    let is_duplicate = ctx_mut(|c| {
        let event_tx = c.event_tx.clone();
        let conn = c
            .connections
            .entry(peer_addr)
            .or_insert_with(|| Connection::new(peer_addr, event_tx, None));
        if conn.from_peer.is_no_connection() {
            conn.from_peer = FromPeer::Established {
                q_conn,
                pending_reads: Default::default(),
            };

            if let ToPeer::Established {
                ref peer_cert_der, ..
            } = conn.to_peer
            {
                let node_info = NodeInfo {
                    peer_addr,
                    peer_cert_der: peer_cert_der.clone(),
                };

                // TODO come back to all the connected-to events and see if we are handling all
                // cases
                let event = if let Some(bootstrap_group_ref) = conn.bootstrap_group_ref.take() {
                    bootstrap_group_ref.terminate_group(true);
                    Event::BootstrappedTo { node: node_info }
                } else {
                    Event::ConnectedTo {
                        peer: node_info.into(),
                    }
                };

                if let Err(e) = c.event_tx.send(event) {
                    info!("ERROR in informing user about a new peer: {:?} - {}", e, e);
                }
            }
            None
        } else {
            Some(q_conn)
        }
    });

    if let Some(_q_conn) = is_duplicate {
        debug!("Not allowing duplicate connection from peer: {}", peer_addr);
        return;
    }

    communicate::read_from_peer(peer_addr, incoming_streams);
}
