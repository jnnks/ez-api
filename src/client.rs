use std::{
    io::{ErrorKind, Read, Write},
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
    Empty,
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
            let buffer = match Self::receive_next(reader) {
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

    fn receive_next(reader: &mut TcpStream) -> Result<Vec<u8>, std::io::Error> {
        // read next packet length
        let mut length_bytes = [0u8; 4];
        reader.read_exact(&mut length_bytes)?;
        let len = u32::from_ne_bytes(length_bytes);

        if len == 0 {
            return Ok(vec![]);
        }

        // read next packet
        let mut message_buffer = vec![0u8; len as usize];
        reader.read_exact(&mut message_buffer)?;
        // println!("client recv buffer {:?}", &message_buffer);
        Ok(message_buffer)
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

            let len = m.len() as u32;
            println!("client sending: {len} {:?}", &m);
            self.writer
                .write_all(&len.to_ne_bytes())
                .map_err(SendError::IoError)?;
            self.writer.write_all(&m).map_err(SendError::IoError)?;
            self.writer.flush().map_err(SendError::IoError)?;

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
