use std::{
    io::{Read, Write},
    marker::PhantomData,
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use crate::{Codec, Continue};

pub enum SendError {
    IoError(std::io::Error),

    Offline,
}

pub enum ReceiveError<TCodecErr> {
    IoError(std::io::Error),
    Empty,
    Codec(TCodecErr),
}

pub struct Client<Tin, Tout> {
    writer: TcpStream,
    online: Arc<AtomicBool>,
    _marker: PhantomData<(Tin, Tout)>,
}

impl<Tin, Tout> Client<Tin, Tout>
where
    Tin: Send + 'static,
    Tout: Send,
{
    /// Connect to a remote server.
    pub async fn connect<C>(
        addr: SocketAddr,
        sink: mpsc::Sender<Result<Tin, ReceiveError<C::TErr>>>,
    ) -> std::io::Result<Self>
    where
        C: Codec<Tin, Tout>,
    {
        let stream = TcpStream::connect(addr)?;
        let (mut reader, writer) = (stream.try_clone().unwrap(), stream);

        let online = Arc::new(AtomicBool::new(true));
        let online2 = online.clone();

        std::thread::spawn(move || {
            loop {
                let buffer = match Self::receive_next(&mut reader) {
                    Ok(buf) => buf,
                    Err(e) => {
                        let _ = sink.send(Err(e));
                        break;
                    }
                };

                let message = match C::decode(buffer) {
                    Ok(m) => m,

                    Err((e, Continue(true))) => {
                        if sink.send(Err(ReceiveError::Codec(e))).is_err() {
                            break;
                        }

                        continue;
                    }

                    Err((e, Continue(false))) => {
                        let _ = sink.send(Err(ReceiveError::Codec(e)));
                        break;
                    }
                };

                if sink.send(Ok(message)).is_err() {
                    break;
                }
            }

            online2.store(false, Ordering::SeqCst);
        });

        Ok(Self {
            writer,
            online,
            _marker: PhantomData,
        })
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
    pub async fn send<C>(&mut self, message: &Tout) -> Result<(), SendError>
    where
        C: Codec<Tin, Tout>,
    {
        if self.online.load(Ordering::SeqCst) {
            let m = C::encode(message);

            self.writer.write_all(m).map_err(SendError::IoError)?;
            self.writer.flush().map_err(SendError::IoError)?;

            Ok(())
        } else {
            Err(SendError::Offline)
        }
    }
}
