use core::time;
use futures::stream::StreamExt as _;
use p2p_test::{
    ClientEvent, ClientId, LogResult as _, Message, MessageClient2Client, MessageClient2Server,
    MessageServer2Client, ServerEvent, Socket,
};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

#[derive(clap::Parser)]
struct Args {
    server_addr: SocketAddr,

    #[clap(long)]
    client_id: Option<String>,

    #[clap(long, default_value = "60")]
    keep_alive_secs: u64,
}

async fn real_main() {
    env_logger::init();

    let Args {
        client_id,
        server_addr,
        keep_alive_secs,
    } = clap::Parser::parse();

    let my_id = client_id
        .as_ref()
        .map(String::as_str)
        .map(ClientId::try_from)
        .and_then(Result::ok)
        .unwrap_or_else(ClientId::rand);

    let socket = Socket::<Message>::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .await
        .unwrap();

    socket
        .send_to(
            server_addr,
            Message::Client2Server(MessageClient2Server::Event(ClientEvent::Hello {
                id: my_id.clone(),
            })),
        )
        .await
        .unwrap();

    let packets = socket.recv_iterator();
    let mut packets = Box::pin(packets);

    let keep_alive_timer = async_io::Timer::interval(time::Duration::from_secs(keep_alive_secs));

    let mut keep_alive_timer = Box::pin(keep_alive_timer.fuse());

    loop {
        futures::select! {
            result = packets.next() => {
                match result.transpose() {
                    Ok(None) => continue,
                    Ok(Some((message, addr))) => {
                        match message {
                            Message::Server2Client(message) => match message {
                                MessageServer2Client::Event(event) => {
                                    match event {
                                        ServerEvent::ClientJoin { id, sock } => {
                                            if id != my_id {
                                                let _ = socket
                                                    .send_to(
                                                        sock,
                                                        Message::Client2Client(
                                                            MessageClient2Client::Ping(uuid::Uuid::now_v7())
                                                        )
                                                    )
                                                    .await
                                                    .log_error();
                                            }
                                        }
                                        ServerEvent::ClientLeave { .. } => {}
                                        ServerEvent::Goodbye => {
                                            // register again
                                            let _ = socket
                                                .send_to(
                                                    server_addr,
                                                    Message::Client2Server(MessageClient2Server::Event(
                                                        ClientEvent::Hello { id: my_id.clone() }
                                                    )),
                                                )
                                                .await
                                                .log_error();
                                        }
                                    }
                                }
                            }
                            Message::Client2Client(message) => match message {
                                MessageClient2Client::Ping(uuid) => {
                                    let _ = socket
                                        .send_to(
                                            addr,
                                            Message::Client2Client(MessageClient2Client::Pong(uuid))
                                        )
                                        .await
                                        .log_error();
                                }
                                MessageClient2Client::Pong(_) => {}
                            }
                            Message::Client2Server(_) => log::error!("cannot handle packet intended for server")
                        }
                    }
                    Err(err) => log::error!("{err:?}")
                }
            }
            _ = keep_alive_timer.next() => {
                let _ = socket
                    .send_to(
                        server_addr,
                        Message::Client2Server(MessageClient2Server::Event(ClientEvent::KeepAlive))
                    )
                    .await
                    .log_error();
            }
        }
    }
}

fn main() {
    let ex = async_executor::LocalExecutor::new();
    let task = ex.spawn(real_main());
    async_io::block_on(ex.run(task));
}
