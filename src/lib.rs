pub mod client;
pub mod server;
pub use client::Client;
pub use server::Server;

pub struct Continue(pub bool);

pub trait Codec<Tin, Tout>: Clone + Send + 'static {
    type TErr: Send + 'static;

    fn encode(&self, outgoing: Tout) -> Vec<u8>;
    fn decode(&self, incoming: Vec<u8>) -> Result<Tin, (Self::TErr, Continue)>;
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use crate::{Client, Codec, Continue, Server, server::Event};

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
    fn test_trivial() {
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let (sink, srv_rx) = std::sync::mpsc::channel();
        let _server: StringServer = Server::bind(addr, StringCodec {}, sink).unwrap();
        let (sink, _clt_rx) = std::sync::mpsc::channel();
        let client: StringClient = Client::connect(addr, StringCodec {}, sink).unwrap();

        assert!(matches!(srv_rx.recv(), Ok((_, Event::Connect))));
        drop(client);
        assert!(matches!(srv_rx.recv(), Ok((_, Event::Disconnect))));
    }
}
