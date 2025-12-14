use std::collections::HashMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::JoinHandle;

use crate::{Codec, Continue};

// These are two ends of the send queue.
// They will accept a callback for every message to be sent, so that the
// call to Server::send only returns, when the data has been handed to the
// TCP socket.
// We forward the operating systems TCP socket buffer limitations
// to the caller of Server::send. If the OS buffer is full, the caller has to
// wait
type ResponseSender = mpsc::SyncSender<Result<(), SendError>>;
type SenderWithResult<Tout> = mpsc::Sender<(ResponseSender, Tout)>;
type ReceiverWithResult<Tout> = mpsc::Receiver<(ResponseSender, Tout)>;

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

    Codec(TCodecErr),
}

pub struct Server<Tin, Tout, C> {
    local_addr: SocketAddr,
    connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
    shutdown: Arc<AtomicBool>,
    active_lock: Arc<RwLock<()>>,
    _marker: PhantomData<(Tin, C)>,
}

impl<Tin, Tout, C> Server<Tin, Tout, C>
where
    Tin: Send + 'static + Debug,
    Tout: Send + 'static + Debug,
    C: Codec<Tin, Tout> + Clone + 'static,
{
    /// Bind a Server to a port.
    ///
    /// Will spawn a backgroud thread to handle incoming connections and two threads for each connected client.
    pub fn bind(
        addr: SocketAddr,
        codec: C,
        sink: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) -> std::io::Result<Self>
    where
        C: Codec<Tin, Tout>,
    {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;

        let connections = Arc::new(RwLock::new(HashMap::new()));
        let connections2 = connections.clone();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown2 = shutdown.clone();

        let active_lock = Arc::new(RwLock::new(()));
        let active_lock2 = active_lock.clone();

        std::thread::spawn(move || {
            let handle = active_lock2.write().unwrap();

            let connection_handles = Arc::new(RwLock::new(vec![]));
            let connection_handles2 = connection_handles.clone();

            Self::accept_connections(
                listener,
                connections2.clone(),
                connection_handles2.clone(),
                sink,
                codec,
                shutdown2,
            );

            // accept_connections will only finish, once it received the shutdown signal.
            // We need to clear exising connections here, so that we do not leak resources.
            // --> all channels will be closed and background threads will terminate
            connections2.write().unwrap().clear();

            // wait for all connection threads to complete gracefully
            for handle in connection_handles2.write().unwrap().drain(..) {
                let _ = handle.join();
            }

            // signal that shutdown is complete
            drop(handle);
        });

        Ok(Self {
            local_addr,
            shutdown,
            active_lock,
            connections,
            _marker: PhantomData,
        })
    }

    /// Send a message to a client.
    pub fn send(&self, who: SocketAddr, value: Tout) -> Result<(), SendError> {
        if let Some(conn) = self.connections.read().unwrap().get(&who) {
            let (tx, rx) = mpsc::sync_channel(1);
            conn.send((tx, value))
                .map_err(|_| SendError::BrokenChannel)?;

            // wait for the message to be handed to the TCP socket
            rx.recv().map_err(|_| SendError::BrokenChannel)?
        } else {
            Err(SendError::AddrNotFound)
        }
    }

    // Send a message to all clients.
    pub fn broadcast(&self, value: Tout)
    where
        Tout: Clone,
    {
        for addr in self.connections.read().unwrap().keys() {
            // ignore result, keep sending to other clients
            let _ = self.send(*addr, value.clone());
        }
    }

    // Returns the local socket address of this listener.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn accept_connections(
        listener: TcpListener,
        connections: Arc<RwLock<HashMap<SocketAddr, SenderWithResult<Tout>>>>,
        connection_handles: Arc<RwLock<Vec<JoinHandle<()>>>>,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
        codec: C,
        shutdown: Arc<AtomicBool>,
    ) {
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }

            match listener.accept() {
                Err(e) => panic!("{e}"), // TODO: what to do here?

                Ok((socket, addr)) => {
                    let connections2 = connections.clone();
                    let data_tx2 = data_tx.clone();
                    let codec2 = codec.clone();
                    let handle = std::thread::spawn(move || {
                        Self::handle_connection(addr, socket, codec2, connections2, data_tx2)
                    });
                    connection_handles.write().unwrap().push(handle);
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
        let (mut reader, writer) = (stream.try_clone().unwrap(), stream);

        // Task to send messages to the client (write half)
        let (client_tx, client_rx) = mpsc::channel();
        connections.write().unwrap().insert(addr, client_tx);
        if data_tx.send((addr, Event::Connect)).is_err() {
            return;
        }

        let codec2 = codec.clone();
        // send and receive all messages
        std::thread::spawn(move || Self::send_messages(client_rx, codec2, writer));
        Self::read_messages(addr, &mut reader, codec, data_tx.clone());

        connections.write().unwrap().remove(&addr);
        let _ = reader.shutdown(std::net::Shutdown::Both);
        let _ = data_tx.send((addr, Event::Disconnect));
    }

    fn read_messages(
        peer: SocketAddr,
        reader: &mut TcpStream,
        codec: C,
        data_tx: mpsc::Sender<(SocketAddr, Event<Tin, C::TErr>)>,
    ) {
        loop {
            let buffer = match crate::read_write::receive_next(reader) {
                Ok(mesg) => mesg,
                Err(e) => {
                    let _ = data_tx.send((peer, Event::Err(ReceiveError::IoError(e))));
                    return;
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
                    return;
                }
            };

            if data_tx.send((peer, Event::Data(message))).is_err() {
                // TODO: main data sink has been dropped, how to handle this better?
                break;
            }
        }
    }

    fn send_messages(
        client_rx: ReceiverWithResult<Tout>,
        codec: C,
        mut writer: TcpStream,
    ) -> Result<(), SendError> {
        while let Ok((tx_result, message)) = client_rx.recv() {
            let m = codec.encode(message);
            let result: Result<(), SendError> =
                crate::read_write::write(&mut writer, &m).map_err(SendError::IoError);

            let _ = tx_result.send(result);
        }
        let _ = writer.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

impl<Tin, Tout, C> Drop for Server<Tin, Tout, C> {
    // Send shutdown signal to acceptor task.
    // This will terminate the accept loop and close all existing connections.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);

        // spawn a connection, to continue the accept_connections loop and check shutdown state
        if let Ok(conn) = TcpStream::connect(self.local_addr) {
            conn.shutdown(std::net::Shutdown::Both).unwrap();
        }

        // wait for accept_connections to drop the lock guard
        // this is a poor-mans condvar
        let _handle = self.active_lock.write();
    }
}
