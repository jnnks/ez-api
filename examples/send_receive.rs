use ez_api::{Client, Codec, Continue, Event, Server};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

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

fn main() {
    type StringServer = Server<String, String, StringCodec>;
    type StringClient = Client<String, String, StringCodec>;

    let any_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0));
    let (sink, srv_rx) = std::sync::mpsc::channel();
    let server: StringServer = Server::bind(any_addr, StringCodec {}, sink).unwrap();

    let (sink, clt_rx) = std::sync::mpsc::channel();
    let mut client: StringClient =
        Client::connect(server.local_addr(), StringCodec {}, sink).unwrap();
    let Ok((addr, Event::Connect)) = srv_rx.recv() else {
        panic!("client did not connect")
    };
    assert_eq!(addr, client.local_addr());

    client.send("ping".into()).unwrap();
    let Ok((addr, Event::Data(msg))) = srv_rx.recv() else {
        panic!("client did not send")
    };
    assert_eq!(&msg, "ping");

    server.send(addr, "pong".into()).unwrap();
    let Ok(Ok(msg)) = clt_rx.recv() else {
        panic!("server did not send")
    };
    assert_eq!(&msg, "pong");
}
