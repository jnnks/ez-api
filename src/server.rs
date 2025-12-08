use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{ErrorKind, Read, Write};
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::time::Duration;

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

pub struct Server<Tin, Tout, C> {
    local_addr: SocketAddr,
    connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
    shutdown: Arc<AtomicBool>,
    _marker: PhantomData<(Tin, C)>,
}

impl<Tin, Tout, C> Server<Tin, Tout, C>
where
    Tin: Send + 'static + Debug,
    Tout: Send + 'static + Debug,
    C: Codec<Tin, Tout> + Clone + 'static,
{
    pub fn bind(
        addr: SocketAddr,
        codec: C,
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
            Self::accept_connections(listener, connections2.clone(), sink, codec, shutdown2);

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

    fn accept_connections(
        listener: TcpListener,
        connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
        codec: C,
        shutdown: Arc<AtomicBool>,
    ) {
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
                    let codec2 = codec.clone();
                    std::thread::spawn(move || {
                        Self::handle_connection(addr, socket, codec2, connections2, data_tx2)
                    });
                }
            }
        }
    }

    fn handle_connection(
        addr: SocketAddr,
        stream: TcpStream,
        codec: C,
        connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) {
        // Split the socket into a read and write half
        let (reader, writer) = (stream.try_clone().unwrap(), stream);

        // Task to send messages to the client (write half)
        let (client_tx, client_rx) = mpsc::channel();
        connections.write().unwrap().insert(addr, client_tx);
        data_tx.send((addr, Event::Connect)).unwrap();

        let codec2 = codec.clone();
        // send and receive all messages
        std::thread::spawn(move || Self::send_messages(client_rx, codec2, writer));
        Self::read_messages(addr, reader, codec, data_tx.clone());

        connections.write().unwrap().remove(&addr);
        data_tx.send((addr, Event::Disconnect)).unwrap();
    }

    fn read_messages(
        peer: SocketAddr,
        mut reader: TcpStream,
        codec: C,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) {
        loop {
            let buffer = match Self::receive_next(&mut reader) {
                Ok(mesg) => mesg,
                Err(e) => {
                    let _ = data_tx.send((peer, Event::Err(ReceiveError::IoError(e))));
                    break;
                }
            };

            let message = match codec.decode(buffer) {
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

            println!("server recv: {:?}", &message);
            if data_tx.send((peer, Event::Data(message))).is_err() {
                break;
            }
        }
        reader.shutdown(std::net::Shutdown::Both).unwrap();
        println!("server receive loop done");
    }

    fn receive_next(reader: &mut TcpStream) -> Result<Vec<u8>, std::io::Error> {
        // read next packet length
        let mut length_bytes = [0u8; 4];
        reader.read_exact(&mut length_bytes)?;
        let len = u32::from_ne_bytes(length_bytes);
        println!("client recv len {len}");

        if len == 0 {
            return Ok(vec![]);
        }

        // read next packet
        let message_buffer = vec![0u8; len as usize];
        reader.read_exact(&mut length_bytes)?;
        Ok(message_buffer)
    }

    fn send_messages(
        client_rx: ReceiverWithResult<Tout>,
        codec: C,
        mut writer: TcpStream,
    ) -> Result<(), SendError> {
        while let Ok((tx_result, message)) = client_rx.recv() {
            let m = codec.encode(message);
            let len = m.len() as u32;
            println!("server sending: {len} {:?}", &m);

            let result: Result<(), SendError> = {
                writer
                    .write_all(&len.to_ne_bytes())
                    .map_err(SendError::IoError)?;

                writer.write_all(&m).map_err(SendError::IoError)?;
                writer.flush().map_err(SendError::IoError)?;

                Ok(())
            };

            let _ = tx_result.send(result);
        }

        Ok(())
    }
}

impl<Tin, Tout, C> Drop for Server<Tin, Tout, C> {
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
