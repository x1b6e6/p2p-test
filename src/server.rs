use p2p_test::{
    ClientEvent, ClientId, LogResult as _, Message, MessageClient2Server, MessageServer2Client,
    ServerEvent, Socket,
};

use async_std::channel::{self, Sender};
use futures::stream::StreamExt as _;
use std::{cell::RefCell, collections::HashMap, net::SocketAddr, time};

#[derive(clap::Parser)]
struct Args {
    listen: SocketAddr,

    #[clap(long, default_value = "300")]
    keep_alive_secs: u64,
}

static mut KEEP_ALIVE: time::Duration = time::Duration::from_secs(300);

struct Client {
    id: ClientId,
    sock: SocketAddr,
    last_update: RefCell<time::Instant>,
}

impl Client {
    pub fn new(id: ClientId, sock: SocketAddr) -> Self {
        Self {
            id,
            sock,
            last_update: RefCell::new(time::Instant::now()),
        }
    }
    pub async fn send(
        &self,
        message: MessageServer2Client,
        tx: &Sender<Command>,
    ) -> anyhow::Result<()> {
        tx.send(Command::Send {
            dst: self.sock,
            message,
        })
        .await
        .map_err(Into::into)
    }

    pub fn is_alive(&self) -> bool {
        self.last_update.borrow().elapsed() < unsafe { KEEP_ALIVE }
    }

    pub fn keep_alive(&self) {
        *self.last_update.borrow_mut() = std::time::Instant::now();
    }
}

#[derive(Default)]
struct ClientPool {
    clients: HashMap<ClientId, Client>,
    sock_id: HashMap<SocketAddr, ClientId>,
}

impl ClientPool {
    pub fn add(&mut self, client: Client) -> Option<Client> {
        self.sock_id.insert(client.sock, client.id);
        self.clients.insert(client.id, client)
    }

    pub fn remove_by_sock(&mut self, sock: &SocketAddr) -> Option<Client> {
        match self.sock_id.remove(sock) {
            Some(id) => self.clients.remove(&id),
            None => None,
        }
    }

    pub fn remove_by_id(&mut self, id: &ClientId) -> Option<Client> {
        let removed = self.clients.remove(id);
        if let Some(ref removed) = removed {
            self.sock_id.remove(&removed.sock);
        }
        removed
    }

    pub fn get_by_sock(&self, sock: &SocketAddr) -> Option<&Client> {
        match self.sock_id.get(sock) {
            Some(id) => self.clients.get(id),
            None => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Client> {
        self.clients.iter().map(|(_id, client)| client)
    }

    pub fn iter_alive(&self) -> impl Iterator<Item = &Client> {
        self.iter().filter(|client| client.is_alive())
    }

    pub async fn broadcast(&self, event: ServerEvent, tx: &Sender<Command>) {
        for client in self.iter_alive() {
            if client.is_alive() {
                let _ = client
                    .send(MessageServer2Client::Event(event), tx)
                    .await
                    .log_error();
            }
        }
    }
}

enum Command {
    Send {
        dst: SocketAddr,
        message: MessageServer2Client,
    },
    Remove {
        id: ClientId,
    },
}

async fn real_main() {
    env_logger::init();

    let Args {
        listen,
        keep_alive_secs,
    } = clap::Parser::parse();

    unsafe { KEEP_ALIVE = time::Duration::from_secs(keep_alive_secs) };

    let socket = Socket::<Message>::bind(listen).await.unwrap();
    let (tx, mut rx) = channel::unbounded::<Command>();
    let mut clients = ClientPool::default();

    let packets = socket.recv_iterator();
    let mut packets = Box::pin(packets);

    loop {
        futures::select! {
            result = packets.next() => {
                match result.transpose() {
                    Ok(None) => continue,
                    Ok(Some((message, sock))) => match message {
                        Message::Client2Server(message) => match message {
                            MessageClient2Server::Event(event) => match event {
                                ClientEvent::Hello { id } => {
                                    let new_client = Client::new(id, sock);
                                    for client in clients.iter() {
                                        if client.is_alive() {
                                            let _ = client
                                                .send(
                                                    MessageServer2Client::Event(ServerEvent::ClientJoin { id, sock }),
                                                    &tx,
                                                )
                                                .await
                                                .log_error();

                                            let _ = new_client
                                                .send(
                                                    MessageServer2Client::Event(ServerEvent::ClientJoin {
                                                        id: client.id,
                                                        sock: client.sock,
                                                    }),
                                                    &tx,
                                                )
                                                .await
                                                .log_error();
                                        } else {
                                            let _ = tx.send(Command::Remove { id: client.id }).await.log_error();
                                        }
                                    }
                                    clients.add(new_client);
                                }
                                ClientEvent::Goodbye => {
                                    if let Some(removed) = clients.remove_by_sock(&sock) {
                                        clients.broadcast(ServerEvent::ClientLeave { id: removed.id }, &tx).await;
                                    }
                                }
                                ClientEvent::KeepAlive => {
                                    match clients.get_by_sock(&sock) {
                                        Some(client) => client.keep_alive(),
                                        None => {
                                            log::warn!("{sock}: keepalive from unknown client");
                                            let _ = tx
                                                .send(Command::Send {
                                                    dst: sock,
                                                    message: MessageServer2Client::Event(ServerEvent::Goodbye)
                                                })
                                                .await
                                                .log_error();
                                        }
                                    }
                                }
                            }
                        }
                        Message::Server2Client(_) | Message::Client2Client(_) => {
                            log::error!("cannot handle message intended for client from {sock}");
                        }
                    }
                    Err(err) => log::error!("{err}")
                }
            }
            result = rx.next() => {
                if let Some(cmd) = result {
                    match cmd {
                        Command::Send { dst, message } => {
                            let _ = socket.send_to(dst, Message::Server2Client(message)).await.log_error();
                        }
                        Command::Remove { id } => {
                            if let Some(removed) = clients.remove_by_id(&id) {
                                let _ = removed
                                    .send(MessageServer2Client::Event(ServerEvent::Goodbye), &tx)
                                    .await
                                    .log_error();

                                clients.broadcast(ServerEvent::ClientLeave{id}, &tx).await;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn main() {
    let ex = async_executor::LocalExecutor::new();
    let task = ex.spawn(real_main());
    async_io::block_on(ex.run(task));
}
