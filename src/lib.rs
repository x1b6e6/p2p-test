use async_std::net::{ToSocketAddrs, UdpSocket};
use futures::stream::{FusedStream, Stream, StreamExt as _};
use rand::RngCore as _;
use rkyv::{
    ser::{serializers::AllocSerializer, Serializer as _},
    validation::validators::DefaultValidator,
    Deserialize as _,
};
use std::{
    cell::RefCell,
    fmt::{Debug, Display},
    marker::PhantomData,
    mem::MaybeUninit,
    net::SocketAddr,
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub struct ClientId([u8; 32]);

impl ClientId {
    pub fn rand() -> Self {
        let mut buf = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut buf);
        Self(buf)
    }
}

impl Debug for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&base_62::encode(&self.0), f)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientIdFromStringError {
    #[error("while decoding from base62: {0}")]
    Decode(base_62::Error),
    #[error("wrong size of client id")]
    WrongSize,
}

impl Into<String> for ClientId {
    fn into(self) -> String {
        base_62::encode(&self.0)
    }
}

impl TryFrom<&str> for ClientId {
    type Error = ClientIdFromStringError;

    fn try_from(client_id: &str) -> std::result::Result<Self, Self::Error> {
        let vec = base_62::decode(client_id).map_err(ClientIdFromStringError::Decode)?;

        if vec.len() != 32 {
            return Err(ClientIdFromStringError::WrongSize);
        }

        let buf = MaybeUninit::<[u8; 32]>::uninit();
        // SAFETY: it's just array of bytes
        let mut buf = unsafe { buf.assume_init() };
        buf.copy_from_slice(&vec);

        Ok(Self(buf))
    }
}

pub trait LogResult: Sized {
    fn log_error(self) -> Self;
}

impl<T, E: std::fmt::Debug> LogResult for Result<T, E> {
    fn log_error(self) -> Self {
        if let Err(ref err) = self {
            log::error!("{err:?}");
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub enum ServerEvent {
    ClientJoin { id: ClientId, sock: SocketAddr },
    ClientLeave { id: ClientId },
    Goodbye,
}

#[derive(Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub enum MessageServer2Client {
    Event(ServerEvent),
}

#[derive(Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub enum ClientEvent {
    Hello { id: ClientId },
    Goodbye,
    KeepAlive,
}

#[derive(Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub enum MessageClient2Server {
    Event(ClientEvent),
}

#[derive(Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub enum MessageClient2Client {
    Ping(uuid::Uuid),
    Pong(uuid::Uuid),
}

#[derive(Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
pub enum Message {
    Server2Client(MessageServer2Client),
    Client2Server(MessageClient2Server),
    Client2Client(MessageClient2Client),
}

pub async fn send_message_to<T>(
    socket: &UdpSocket,
    addr: SocketAddr,
    message: T,
) -> anyhow::Result<()>
where
    T: rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<0>> + Debug,
{
    log::debug!("{addr} <- {message:?}");
    let mut serializer = AllocSerializer::<0>::default();
    serializer.serialize_value(&message)?;
    let value = serializer.into_serializer().into_inner();

    log::trace!("{addr} <- {value:?}");

    let len = socket.send_to(&value, addr).await?;
    assert_eq!(len, value.len());

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum RecvMessageError {
    #[error("while reading data from socket: {0}")]
    Io(async_std::io::Error),
    #[error("while deserialize packet: {0}")]
    Check(String),
}

pub async fn recv_message_from<T>(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(T, SocketAddr)>, RecvMessageError>
where
    T: rkyv::Archive + Debug,
    for<'a> <T as rkyv::Archive>::Archived:
        rkyv::Deserialize<T, rkyv::Infallible> + rkyv::CheckBytes<DefaultValidator<'a>>,
{
    let (len, addr) = socket.recv_from(buf).await.map_err(RecvMessageError::Io)?;
    if len == 0 {
        return Ok(None);
    }

    let buf = &buf[0..len];
    log::trace!("{addr} -> {buf:?}");

    let archived = rkyv::check_archived_root::<T>(buf)
        .map_err(|err| RecvMessageError::Check(format!("{err}")))?;
    let value = archived.deserialize(&mut rkyv::Infallible).unwrap();

    log::debug!("{addr} -> {value:?}");

    Ok(Some((value, addr)))
}

pub struct Socket<T> {
    socket: UdpSocket,
    buf: RefCell<[u8; 1024]>,
    _phantom: PhantomData<T>,
}

impl<T> Socket<T> {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            buf: RefCell::new([0u8; 1024]),
            _phantom: PhantomData,
        }
    }

    pub async fn bind<A>(addr: A) -> anyhow::Result<Self>
    where
        A: ToSocketAddrs,
    {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self::new(socket))
    }

    pub async fn send_to(&self, addr: SocketAddr, message: T) -> anyhow::Result<()>
    where
        T: rkyv::Serialize<rkyv::ser::serializers::AllocSerializer<0>> + Debug,
    {
        send_message_to(&self.socket, addr, message).await
    }

    pub async fn recv_from(&self) -> Result<Option<(T, SocketAddr)>, RecvMessageError>
    where
        T: rkyv::Archive + Debug,
        for<'a> <T as rkyv::Archive>::Archived:
            rkyv::Deserialize<T, rkyv::Infallible> + rkyv::CheckBytes<DefaultValidator<'a>>,
    {
        recv_message_from(&self.socket, self.buf.borrow_mut().as_mut_slice()).await
    }

    pub fn recv_iterator(
        &self,
    ) -> impl Stream<Item = Result<(T, SocketAddr), RecvMessageError>> + FusedStream + '_
    where
        T: rkyv::Archive + Debug,
        for<'a> <T as rkyv::Archive>::Archived:
            rkyv::Deserialize<T, rkyv::Infallible> + rkyv::CheckBytes<DefaultValidator<'a>>,
    {
        let iter = async_gen::gen! {
            loop {
                let result = self.recv_from().await;
                match result {
                    Ok(None) => { continue },
                    Ok(Some(result)) => { yield Ok(result) },
                    Err(err) => { yield Err(err) },
                };
            }
        };

        iter.fuse()
    }
}
