pub mod client;
pub mod server;

pub use client::Client;

pub struct Continue(pub bool);

pub trait Codec<Tin, Tout> {
    type TErr: Send + 'static;

    fn encode(outgoing: &Tout) -> &[u8];
    fn decode(incoming: Vec<u8>) -> Result<Tin, (Self::TErr, Continue)>;
}

// fn read_json<Tin, S>(_source: &mut S) -> Result<Tin, ReceiveError>
// where
//     Tin: DeserializeOwned,
//     S: Read,
// {
//     todo!()
//     // // read next packet length
//     // let mut length_bytes = [0u8; 4];
//     // source.read_exact(&mut length_bytes).await?;
//     // let len = u32::from_be_bytes(length_bytes);

//     // if len == 0 {
//     //     // return early when receiving data of size 0
//     //     return Err(ReceiveError::ProtocolError);
//     // }

//     // // read next packet
//     // let mut mesage_buffer = vec![0u8; len as usize];
//     // source.read_exact(&mut mesage_buffer).await?;
//     // match serde_json::from_slice(&mesage_buffer) {
//     //     Ok(json) => Ok(json),
//     //     Err(e) => {
//     //         error!("Error while reading JSON data:\n\n{e}\n\n{JSON_DECODE_ERROR}");
//     //         Err(ReceiveError::JsonError(e))
//     //     }
//     // }
// }

// fn write_json<Tout, S>(_sink: &mut S, _value: Tout) -> Result<(), SendError>
// where
//     Tout: Serialize,
//     S: Write,
// {
//     todo!()
//     // let json_bytes = serde_json::to_vec(&value)?;
//     // let length_bytes = u32::to_be_bytes(json_bytes.len() as u32);
//     // sink.write_all(&length_bytes).await?;
//     // sink.write_all(&json_bytes).await?;
//     // Ok(())
// }
