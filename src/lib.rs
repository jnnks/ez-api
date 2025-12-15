//! Simple MPSC TCP Message Queues

mod client;
mod server;
use std::fmt::Debug;

pub use client::Client;
pub use server::{Event, Server};

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
    use std::{
        io::ErrorKind,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
        time::Duration,
    };

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

    const ANY_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));

    #[test]
    fn test_connect_disconnect_1() {
        // establish connection and drop client

        let (sink, srv_rx) = std::sync::mpsc::channel();
        let server: StringServer = Server::bind(ANY_ADDR, StringCodec {}, sink).unwrap();
        let (sink, _clt_rx) = std::sync::mpsc::channel();
        let client: StringClient =
            Client::connect(server.local_addr(), StringCodec {}, sink).unwrap();
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
        // establish connection and drop server

        let (sink, srv_rx) = std::sync::mpsc::channel();
        let server: StringServer = Server::bind(ANY_ADDR, StringCodec {}, sink).unwrap();

        let (sink, clt_rx) = std::sync::mpsc::channel();
        let client: StringClient =
            Client::connect(server.local_addr(), StringCodec {}, sink).unwrap();
        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));

        drop(server);

        assert!(matches!(
            clt_rx.recv().unwrap(),
            Err(client::ReceiveError::IoError(_))
        ));
        assert!(!client.online());
    }

    #[test]
    fn test_connect_disconnect_3() {
        // establish connection and drop server main data channel

        let (sink, srv_rx) = std::sync::mpsc::channel();
        let server: StringServer = Server::bind(ANY_ADDR, StringCodec {}, sink).unwrap();

        let (sink, clt_rx) = std::sync::mpsc::channel();
        let mut client: StringClient =
            Client::connect(server.local_addr(), StringCodec {}, sink).unwrap();
        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));

        drop(srv_rx);

        // sending data will make server try to send into srv_rx, this will fail and the connection
        // is shutdown
        client.send("hello".into()).unwrap();
        assert!(matches!(
            clt_rx.recv().unwrap(),
            Err(client::ReceiveError::IoError(_))
        ));
        assert!(!client.online());
    }

    #[test]
    fn test_connect_no_server() {
        // try connecting to a port without server

        let (sink, _clt_rx) = std::sync::mpsc::channel();
        let result: Result<Client<String, String, StringCodec>, std::io::Error> =
            Client::connect(ANY_ADDR, StringCodec {}, sink);

        let Err(e) = result else {
            panic!("did not return Err(_)")
        };
        assert_eq!(ErrorKind::ConnectionRefused, e.kind());
    }

    #[test]
    fn test_connect_cancel_message() {
        #[derive(Clone)]
        struct RawCodec;
        impl Codec<Vec<u8>, Vec<u8>> for RawCodec {
            type TErr = String;
            fn encode(&self, outgoing: Vec<u8>) -> Vec<u8> {
                outgoing
            }
            fn decode(&self, incoming: Vec<u8>) -> Result<Vec<u8>, (Self::TErr, Continue)> {
                Ok(incoming)
            }
        }

        let (sink, _srv_rx) = std::sync::mpsc::channel();
        let server: Server<Vec<u8>, Vec<u8>, RawCodec> =
            Server::bind(ANY_ADDR, RawCodec {}, sink).unwrap();

        let (sink, _clt_rx) = std::sync::mpsc::channel();
        let mut client: Client<Vec<u8>, Vec<u8>, RawCodec> =
            Client::connect(server.local_addr(), RawCodec {}, sink).unwrap();
        // send empty message
        client.send(vec![]).unwrap();

        std::thread::sleep(Duration::from_millis(100));
        assert!(!client.online());
    }

    #[test]
    fn test_send_recv_1() {
        let (sink, srv_rx) = std::sync::mpsc::channel();
        let server: StringServer = Server::bind(ANY_ADDR, StringCodec {}, sink).unwrap();

        let (sink, clt_rx) = std::sync::mpsc::channel();
        let mut client: StringClient =
            Client::connect(server.local_addr(), StringCodec {}, sink).unwrap();
        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));

        client.send("hello".into()).unwrap();
        let Ok((_, Event::Data(data))) = srv_rx.recv() else {
            panic!("recv failed")
        };
        assert_eq!("hello", &data);

        server.send(client.local_addr(), "hello".into()).unwrap();
        let Ok(Ok(msg)) = clt_rx.recv() else {
            panic!("recv failed")
        };
        assert_eq!("hello", &msg);
    }

    #[test]
    fn test_send_recv_2() {
        let (sink, srv_rx) = std::sync::mpsc::channel();
        let server: StringServer = Server::bind(ANY_ADDR, StringCodec {}, sink).unwrap();

        let (sink, clt_rx) = std::sync::mpsc::channel();
        let mut client: StringClient =
            Client::connect(server.local_addr(), StringCodec {}, sink).unwrap();
        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));

        for _ in 0..100 {
            client.send("hello".into()).unwrap();
        }

        for _ in 0..100 {
            let Ok((_, Event::Data(data))) = srv_rx.recv() else {
                panic!("recv failed")
            };
            assert_eq!("hello", &data);
        }

        for _ in 0..100 {
            server.send(client.local_addr(), "hello".into()).unwrap();
        }

        for _ in 0..100 {
            let Ok(Ok(msg)) = clt_rx.recv() else {
                panic!("recv failed")
            };
            assert_eq!("hello", &msg);
        }
    }
}
