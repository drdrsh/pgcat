use std::cmp::{max, min};
use std::time::Duration;

/// Implementation of the PostgreSQL server (database) protocol.
/// Here we are pretending to the a Postgres client.
use bytes::{Bytes, BytesMut};

use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::time::sleep;

use crate::config::{Address, Role, User};
use crate::messages::flush;
use crate::pool::ClientServerMap;
use crate::server::Server;
use crate::stats::get_reporter;
use log::{error, info, trace};

const MAX_CONNECT_RETRIES: u32 = 5;
const MAX_SEND_RETRIES: u32 = 3;

pub enum MirrorOperation {
    Send(Bytes),
    Receive,
}
pub struct MirroredClient {
    pub address: Address,
    pub user: User,
    pub database: String,
    pub bytes_rx: Receiver<MirrorOperation>,
    pub exit_rx: Receiver<()>,
    successful_sends_without_recv: u32
}

impl MirroredClient {

    async fn connect_with_retries(&mut self, server_id: i32) -> Option<Server> {
        let mut delay = Duration::from_secs(0);
        let min_backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);
        let mut retries = 0;

        loop {
            let server = self.connect(server_id).await;
            if server.is_some() {
                let tmp_server: Server = server.unwrap();
                if !tmp_server.is_bad() {
                    break Some(tmp_server);
                }
            }
            delay = max(min_backoff, delay);
            delay = min(max_backoff, delay * 2);
            retries += 1;
            if retries > MAX_CONNECT_RETRIES {
                break None;
            }
            sleep(delay).await;
        }
    }

    async fn connect(&mut self, server_id: i32) -> Option<Server> {
        self.successful_sends_without_recv = 0;
        let stats = get_reporter();
        stats.server_register(
            server_id,
            self.address.id,
            self.address.name(),
            self.database.clone(),
            self.address.username.clone(),
        );
        stats.server_login(server_id);

        match Server::startup(
            server_id,
            &self.address.clone(),
            &self.user.clone(),
            self.database.as_str(),
            ClientServerMap::default(),
            get_reporter(),
        )
        .await
        {
            Ok(conn) => {
                stats.server_idle(server_id);
                Some(conn)
            }
            Err(_) => {
                stats.server_disconnecting(server_id);
                None
            }
        }
    }

    pub fn start(mut self, server_id: i32) {
        let address = self.address.clone();
        let mut server_optional = None;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = self.exit_rx.recv() => {
                        info!("Got mirror exit signal, exiting {:?}", address.clone());
                        break;
                    }

                    op = self.bytes_rx.recv() => {
                        if server_optional.is_none() {
                            server_optional = self.connect_with_retries(server_id).await;
                            if server_optional.is_none() {
                                error!("Failed to connect to mirror, Discarding message {:?}", address.clone());
                                continue;
                            }
                        }

                        let mut server = server_optional.unwrap();
                        if server.is_bad() {
                            server_optional = self.connect_with_retries(server_id).await;
                            if server_optional.is_none() {
                                error!("Failed to connect to mirror, Discarding message {:?}", address.clone());
                                continue;
                            }
                            server = server_optional.unwrap();
                        }

                        match op {
                            Some(MirrorOperation::Send(bytes)) => {
                                // Retry sending up to MAX_SEND_RETRIES times
                                let mut retries = 0;
                                loop {
                                    match server.send(&BytesMut::from(&bytes[..])).await {
                                        Ok(_) => {
                                            trace!("Sent to mirror: {} {:?}", String::from_utf8_lossy(&bytes[..]), address.clone());
                                            self.successful_sends_without_recv += 1;
                                            break;
                                        }
                                        Err(err) => {
                                            if retries > MAX_SEND_RETRIES {
                                                error!("Failed to send to mirror, Discarding message {:?}, {:?}", err, address.clone());
                                                break;
                                            } else {
                                                error!("Failed to send to mirror, retrying {:?}, {:?}", err, address.clone());
                                                retries += 1;
                                                server_optional = self.connect_with_retries(server_id).await;
                                                if server_optional.is_none() {
                                                    error!("Failed to connect to mirror, Discarding message {:?}", address.clone());
                                                    continue;
                                                }
                                                server = server_optional.unwrap();
                                                continue;
                                            }
                                        }
                                    }
                                }
                            }

                            Some(MirrorOperation::Receive) => {
                                /*
                                    Receiving from the server is best effort.
                                    We don't really care about the response but we do not want
                                    to leave the data lingering in Postgres send buffer.

                                    To avoid blocking on recv forever, we keep track of the number of
                                    successful sends without a recv. If we have not sent anything
                                    we shouldn't expect to receive anything. We also send a sync
                                    message to gaurantee a response from the server.
                                 */

                                if self.successful_sends_without_recv >= 5 {
                                    // We send a flush message to gaurantee a server response
                                    if server.send(&flush()).await.is_err() {
                                        error!("Failed to send flush to mirror, disconnecting {:?}", address.clone());
                                        server_optional = None; // Force reconnect
                                        continue;
                                    }
                                    match server.recv().await {
                                        Ok(_) => {
                                            trace!("Received from mirror: {:?}", address.clone());
                                            self.successful_sends_without_recv = 0;
                                        }
                                        Err(_) => {
                                            error!("Failed to receive from mirror, disconnecting {:?}", address.clone());
                                        }
                                    }
                                }
                            }
                            None => {
                                info!("Mirror channel closed, exiting {:?}", address.clone());
                                break;
                            }
                        }
                        server_optional = Some(server);
                    }
                }
            }
        });
    }
}
pub struct MirroringManager {
    pub byte_senders: Vec<Sender<MirrorOperation>>,
    pub exit_senders: Vec<Sender<()>>,
}
impl MirroringManager {
    pub fn from_addresses(
        user: User,
        database: String,
        addresses: Vec<Address>,
    ) -> MirroringManager {
        let mut byte_senders: Vec<Sender<MirrorOperation>> = vec![];
        let mut exit_senders: Vec<Sender<()>> = vec![];

        addresses.iter().for_each(|mirror| {
            let (bytes_tx, bytes_rx) = channel::<MirrorOperation>(500);
            let (exit_tx, exit_rx) = channel::<()>(1);
            let mut addr = mirror.clone();
            addr.role = Role::Mirror;
            let client = MirroredClient {
                user: user.clone(),
                database: database.to_owned(),
                address: addr,
                bytes_rx,
                exit_rx,
                successful_sends_without_recv: 0,
            };
            exit_senders.push(exit_tx.clone());
            byte_senders.push(bytes_tx.clone());
            client.start(rand::random::<i32>());
        });

        Self {
            byte_senders: byte_senders,
            exit_senders: exit_senders,
        }
    }

    pub fn send(self: &mut Self, bytes: &BytesMut) {
        let cpy = bytes.clone().freeze();
        self.byte_senders.iter_mut().for_each(|sender| {
            match sender.try_send(MirrorOperation::Send(cpy.clone())) {
                Ok(_) => {}
                Err(_) => {}
            }
        });
    }

    pub fn receive(self: &mut Self) {
        self.byte_senders.iter_mut().for_each(|sender| {
            match sender.try_send(MirrorOperation::Receive) {
                Ok(_) => {}
                Err(_) => {}
            }
        });
    }

    pub fn exit(self: &mut Self) {
        self.exit_senders
            .iter_mut()
            .for_each(|sender| match sender.try_send(()) {
                Ok(_) => {}
                Err(_) => {}
            });
    }
}
