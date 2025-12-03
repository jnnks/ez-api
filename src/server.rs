use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{ErrorKind, Read, Write};
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};

use crate::{Codec, Continue};

// These are two ends of the send queue.
// They will accept a callback for every message to be sent, so that the
// call to Server::send only returns, when the data has been handed to the
// TCP socket.
// We forward the operating systems TCP socket buffer limitations
// to the caller of Server::send. If the OS buffer is full, the caller has to
// wait
type SenderWithResult<Tout> = mpsc::Sender<(mpsc::Sender<Result<(), SendError>>, Tout)>;
type ReceiverWithResult<Tout> = mpsc::Receiver<(mpsc::Sender<Result<(), SendError>>, Tout)>;

#[derive(Debug)]
pub enum Event<Trecv, TCodecErr> {
    Connect,
    Data(Trecv),
    Err(ReceiveError<TCodecErr>),
    Disconnect,
}

#[derive(Debug)]
pub enum SendError {
    IoError(std::io::Error),
    BrokenChannel,
    AddrNotFound,
}

#[derive(Debug)]
pub enum ReceiveError<TCodecErr> {
    IoError(std::io::Error),

    Empty,

    Codec(TCodecErr),
}

pub struct Server<Tin, Tout> {
    local_addr: SocketAddr,
    connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
    shutdown: Arc<AtomicBool>,
    _marker: PhantomData<Tin>,
}

impl<Tin, Tout> Server<Tin, Tout>
where
    Tin: Send + 'static,
    Tout: Send + 'static,
{
    pub fn bind<C>(
        addr: SocketAddr,
        sink: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) -> std::io::Result<Self>
    where
        C: Codec<Tin, Tout>,
    {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;

        // fetch local addr in case we were bound to any port
        let local_addr = listener
            .local_addr()
            .expect("Cannot Retrieve Local Address");

        let connections = Arc::new(RwLock::new(HashMap::new()));
        let connections2 = connections.clone();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown2 = shutdown.clone();

        std::thread::spawn(move || {
            Self::accept_connections::<C>(listener, connections2.clone(), sink, shutdown2);

            // accept_connections will only finish, once it received the shutdown signal.
            // We need to clear exising connections here, so that we do not leak resources.
            connections2.write().unwrap().clear();
        });

        Ok(Self {
            local_addr,
            shutdown,
            connections,
            _marker: PhantomData,
        })
    }

    pub fn send(&self, who: SocketAddr, value: Tout) -> Result<(), SendError> {
        if let Some(conn) = self.connections.read().unwrap().get(&who) {
            // We build a oneshot channel to make sure, the message was sent.
            //   The channel will receive either "()" or a SendError, which will
            //   be forwarded to the caller of this function.
            let (tx, rx) = mpsc::channel();
            conn.send((tx, value))
                .map_err(|_| SendError::BrokenChannel)?;

            // wait for the message to be handed to the TCP socket
            rx.recv().map_err(|_| SendError::BrokenChannel)?
        } else {
            Err(SendError::AddrNotFound)
        }
    }

    pub fn broadcast(&self, value: Tout)
    where
        Tout: Clone,
    {
        for addr in self.connections.read().unwrap().keys() {
            // ignore result, keep sending to other clients
            let _ = self.send(*addr, value.clone());
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn accept_connections<C>(
        listener: TcpListener,
        connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
        shutdown: Arc<AtomicBool>,
    ) where
        C: Codec<Tin, Tout>,
    {
        loop {
            match listener.accept() {
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        if shutdown.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    e => panic!("{:?}", e),
                },

                Ok((socket, addr)) => {
                    let connections2 = connections.clone();
                    let data_tx2 = data_tx.clone();
                    std::thread::spawn(move || {
                        Self::handle_connection::<C>(addr, socket, connections2, data_tx2)
                    });
                }
            }
        }
    }

    fn handle_connection<C>(
        addr: SocketAddr,
        stream: TcpStream,
        connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) where
        C: Codec<Tin, Tout>,
    {
        // Split the socket into a read and write half
        let (reader, writer) = (stream.try_clone().unwrap(), stream);

        // Task to send messages to the client (write half)
        let (client_tx, client_rx) = mpsc::channel();
        connections.write().unwrap().insert(addr, client_tx);
        data_tx.send((addr, Event::Connect)).unwrap();

        // send and receive all messages
        std::thread::spawn(move || Self::send_messages::<C>(client_rx, writer));
        Self::read_messages::<C>(addr, reader, data_tx.clone());

        connections.write().unwrap().remove(&addr);
        data_tx.send((addr, Event::Disconnect)).unwrap();
    }

    fn read_messages<C>(
        peer: SocketAddr,
        mut reader: TcpStream,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) where
        C: Codec<Tin, Tout>,
    {
        loop {
            let buffer = match Self::receive_next(&mut reader) {
                Ok(buf) => buf,
                Err(e) => {
                    let _ = data_tx.send((peer, Event::Err(e)));
                    break;
                }
            };

            let message = match C::decode(buffer) {
                Ok(m) => m,

                Err((e, Continue(true))) => {
                    if data_tx
                        .send((peer, Event::Err(ReceiveError::Codec(e))))
                        .is_err()
                    {
                        break;
                    }

                    continue;
                }

                Err((e, Continue(false))) => {
                    let _ = data_tx.send((peer, Event::Err(ReceiveError::Codec(e))));
                    break;
                }
            };

            if data_tx.send((peer, Event::Data(message))).is_err() {
                break;
            }
        }
    }

    fn receive_next<CErr>(reader: &mut TcpStream) -> Result<Vec<u8>, ReceiveError<CErr>> {
        // read next packet length
        let mut length_bytes = [0u8; 4];
        reader
            .read_exact(&mut length_bytes)
            .map_err(|e| ReceiveError::IoError(e))?;
        let len = u32::from_be_bytes(length_bytes);

        if len == 0 {
            return Err(ReceiveError::Empty);
        }

        // read next packet
        let mut message_buffer = vec![0u8; len as usize];
        reader
            .read_exact(&mut message_buffer)
            .map_err(|e| ReceiveError::IoError(e))?;

        Ok(message_buffer)
    }

    fn send_messages<C>(
        client_rx: ReceiverWithResult<Tout>,
        mut writer: TcpStream,
    ) -> Result<(), SendError>
    where
        C: Codec<Tin, Tout>,
    {
        while let Ok((tx_result, message)) = client_rx.recv() {
            let m = C::encode(&message);

            let result: Result<(), SendError> = {
                writer.write_all(m).map_err(SendError::IoError)?;
                writer.flush().map_err(SendError::IoError)?;

                Ok(())
            };

            let _ = tx_result.send(result);
        }

        Ok(())
    }
}

impl<Tin, Tout> Drop for Server<Tin, Tout> {
    fn drop(&mut self) {
        // Send shutdown signal to acceptor task.
        //   This will terminate the accept loop and close all exisitng connections.
        //   Clearing exising connections is important, as the TcpStreams will continue to live,
        //      even when there is no real access to them anymore.
        // NOTE: We ignore the return value, since SendErrors only occur, when the receiver has been dropped.
        //       In that case, the background task is already terminated.
        self.shutdown.store(true, Ordering::SeqCst);
    }
}
