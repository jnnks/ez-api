use std::{
    io::ErrorKind,
    marker::PhantomData,
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use crate::{Codec, Continue};

#[derive(Debug)]
pub enum SendError {
    IoError(std::io::Error),

    Offline,
}

#[derive(Debug)]
pub enum ReceiveError<TCodecErr> {
    IoError(std::io::Error),
    Codec(TCodecErr),
}

pub struct Client<Tin, Tout, C> {
    writer: TcpStream,
    online: Arc<AtomicBool>,
    codec: C,
    _marker: PhantomData<(Tin, Tout)>,
}

impl<Tin, Tout, C> Client<Tin, Tout, C>
where
    Tin: Send + 'static,
    Tout: Send,
    C: Codec<Tin, Tout>,
{
    /// Connect to a remote server.
    pub fn connect(
        addr: SocketAddr,
        codec: C,
        sink: mpsc::Sender<Result<Tin, ReceiveError<C::TErr>>>,
    ) -> std::io::Result<Self>
    where
        C: Codec<Tin, Tout>,
    {
        let stream = TcpStream::connect(addr)?;
        let (mut reader, writer) = (stream.try_clone().unwrap(), stream);

        let online = Arc::new(AtomicBool::new(true));
        let online2 = online.clone();
        let codec2 = codec.clone();

        std::thread::spawn(move || {
            Self::receive_messages(&mut reader, codec2, sink);

            online2.store(false, Ordering::SeqCst);
            match reader.shutdown(std::net::Shutdown::Both) {
                Ok(()) => { /* ok */ }
                Err(e) if e.kind() == ErrorKind::NotConnected => { /* ignore */ }
                Err(e) => panic!("Failed to shutdown reader half: {e}"),
            }
        });

        Ok(Self {
            writer,
            online,
            codec,
            _marker: PhantomData,
        })
    }

    fn receive_messages(
        reader: &mut TcpStream,
        codec: C,
        sink: mpsc::Sender<Result<Tin, ReceiveError<C::TErr>>>,
    ) {
        loop {
            let buffer = match crate::read_write::receive_next(reader) {
                Ok(buf) => buf,
                Err(e) => {
                    let _ = sink.send(Err(ReceiveError::IoError(e)));
                    break;
                }
            };

            let message = match codec.decode(buffer) {
                Ok(m) => m,

                Err((e, Continue(true))) => {
                    if sink.send(Err(ReceiveError::Codec(e))).is_err() {
                        break;
                    }

                    continue;
                }

                Err((e, Continue(false))) => {
                    // we will break anyway, so we can also ignore the sink.send error
                    let _ = sink.send(Err(ReceiveError::Codec(e)));
                    break;
                }
            };

            if sink.send(Ok(message)).is_err() {
                break;
            }
        }
    }

    /// Return whether the connection is healthy
    pub fn online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }

    // Returns the server address
    pub fn peer_addr(&self) -> SocketAddr {
        self.writer
            .peer_addr()
            .expect("Cannot Retrieve Peer Address")
    }

    /// Send a Json-serializable message
    pub fn send(&mut self, message: Tout) -> Result<(), SendError> {
        if self.online.load(Ordering::SeqCst) {
            let m = self.codec.encode(message);
            crate::read_write::write(&mut self.writer, &m).map_err(SendError::IoError)?;

            Ok(())
        } else {
            Err(SendError::Offline)
        }
    }
}

impl<Tin, Tout, C> Drop for Client<Tin, Tout, C> {
    fn drop(&mut self) {
        self.writer.shutdown(std::net::Shutdown::Both).unwrap();
    }
}
