pub mod client;
pub mod server;
use std::fmt::Debug;

pub use client::Client;
pub use server::Server;

pub struct Continue(pub bool);

pub trait Codec<Tin, Tout>: Clone + Send + 'static {
    type TErr: Send + Debug + 'static;

    fn encode(&self, outgoing: Tout) -> Vec<u8>;
    fn decode(&self, incoming: Vec<u8>) -> Result<Tin, (Self::TErr, Continue)>;
}

mod read_write {
    use std::io::{Read, Write};

    pub fn receive_next(reader: &mut std::net::TcpStream) -> Result<Vec<u8>, std::io::Error> {
        let len = {
            // read next packet length
            let mut length_bytes = [0u8; 4];
            reader.read_exact(&mut length_bytes)?;
            u32::from_ne_bytes(length_bytes)
        };

        if len == 0 {
            return Ok(vec![]);
        }

        let message_buffer = {
            // read next packet
            let mut message_buffer = vec![0u8; len as usize];
            reader.read_exact(&mut message_buffer)?;
            message_buffer
        };
        Ok(message_buffer)
    }

    pub fn write(sink: &mut std::net::TcpStream, buf: &[u8]) -> Result<(), std::io::Error> {
        let len = buf.len() as u32;
        sink.write_all(&len.to_ne_bytes())?;
        sink.write_all(&buf)?;
        sink.flush()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use crate::{
        Client, Codec, Continue, Server, client,
        server::{Event, ReceiveError},
    };

    #[derive(Clone)]
    struct StringCodec;
    impl Codec<String, String> for StringCodec {
        type TErr = String;
        fn encode(&self, outgoing: String) -> Vec<u8> {
            outgoing.into_bytes()
        }
        fn decode(&self, incoming: Vec<u8>) -> Result<String, (String, Continue)> {
            Ok(String::from_utf8(incoming).unwrap())
        }
    }

    type StringServer = Server<String, String, StringCodec>;
    type StringClient = Client<String, String, StringCodec>;

    #[test]
    fn test_connect_disconnect_1() {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let (sink, srv_rx) = std::sync::mpsc::channel();
        let _server: StringServer = Server::bind(addr, StringCodec {}, sink).unwrap();
        let (sink, _clt_rx) = std::sync::mpsc::channel();
        let client: StringClient = Client::connect(addr, StringCodec {}, sink).unwrap();

        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));
        drop(client);
        assert!(matches!(
            srv_rx.recv(),
            Ok((_, Event::Err(ReceiveError::IoError(_))))
        ));
        assert!(matches!(srv_rx.recv(), Ok((_, Event::Disconnect))));
    }

    #[test]
    fn test_connect_disconnect_2() {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let (sink, srv_rx) = std::sync::mpsc::channel();

        let server: StringServer = Server::bind(addr, StringCodec {}, sink).unwrap();

        let (sink, clt_rx) = std::sync::mpsc::channel();
        let client: StringClient = Client::connect(addr, StringCodec {}, sink).unwrap();

        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));
        drop(server);

        assert!(matches!(
            clt_rx.recv().unwrap(),
            Err(client::ReceiveError::IoError(_))
        ));
        assert!(!client.online());
    }
}
