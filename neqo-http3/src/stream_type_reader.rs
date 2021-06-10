// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![allow(clippy::module_name_repetitions)]

use crate::control_stream_local::HTTP3_UNI_STREAM_TYPE_CONTROL;
use crate::hframe::H3_FRAME_TYPE_HEADERS;
use crate::wt_stream::{WEBTRANSPORT_STREAM, WEBTRANSPORT_UNI_STREAM};
use crate::{
    AppError, Error, Http3StreamType, HttpRecvStream, NewStreamType, ReceiveOutput, RecvStream,
    Res, ResetType, WtRecvStream,
};
use neqo_common::{qtrace, Decoder, IncrementalDecoderUint};
use neqo_qpack::decoder::QPACK_UNI_STREAM_TYPE_DECODER;
use neqo_qpack::encoder::QPACK_UNI_STREAM_TYPE_ENCODER;
use neqo_transport::{Connection, StreamId, StreamType};

pub const HTTP3_UNI_STREAM_TYPE_PUSH: u64 = 0x1;

#[derive(Debug, PartialEq)]
pub enum NewStreamTypeReader {
    ReadType {
        reader: IncrementalDecoderUint,
        stream_id: u64,
    },
    ReadWtSessionId {
        reader: IncrementalDecoderUint,
        stream_id: u64,
    },
    Done,
}

impl NewStreamTypeReader {
    pub fn new(stream_id: u64) -> Self {
        NewStreamTypeReader::ReadType {
            reader: IncrementalDecoderUint::default(),
            stream_id,
        }
    }

    fn read(&mut self, conn: &mut Connection) -> Res<(Option<u64>, bool)> {
        match self {
            NewStreamTypeReader::ReadType { reader, stream_id }
            | NewStreamTypeReader::ReadWtSessionId {
                reader, stream_id, ..
            } => loop {
                let to_read = reader.min_remaining();
                let mut buf = vec![0; to_read];
                match conn.stream_recv(*stream_id, &mut buf[..])? {
                    (0, f) => break Ok((None, f)),
                    (amount, f) => {
                        let res = reader.consume(&mut Decoder::from(&buf[..amount]));
                        if res.is_some() || f {
                            break Ok((res, f));
                        }
                    }
                }
            },
            _ => Ok((None, false)),
        }
    }

    pub fn get_type(&mut self, conn: &mut Connection) -> Res<Option<NewStreamType>> {
        loop {
            let (output, fin) = self.read(conn)?;
            if output.is_none() {
                if fin {
                    *self = NewStreamTypeReader::Done;
                }
                break Ok(None);
            }
            let output = output.unwrap();
            qtrace!("Decoded uint {}", output);
            match self {
                NewStreamTypeReader::ReadType { stream_id, .. } => {
                    let res = decode_stream_type(output, StreamId::new(*stream_id).stream_type())?;
                    if fin {
                        *self = NewStreamTypeReader::Done;
                        return map_stream_fin(res);
                    }
                    qtrace!("Decoded stream type {:?}", res);
                    if res.is_some() {
                        *self = NewStreamTypeReader::Done;
                        break Ok(res);
                    }
                    // This is a WebTransportStream stream and it needs more data to be decoded.
                    *self = NewStreamTypeReader::ReadWtSessionId {
                        reader: IncrementalDecoderUint::default(),
                        stream_id: *stream_id,
                    };
                }
                NewStreamTypeReader::ReadWtSessionId { .. } => {
                    *self = NewStreamTypeReader::Done;
                    qtrace!("New WebTransport stream session={}", output);
                    break Ok(Some(NewStreamType::WebTransportStream(output)));
                }
                _ => unreachable!("Cannot be in state NewStreamTypeReader::Done"),
            }
        }
    }
}

fn map_stream_fin(decoded: Option<NewStreamType>) -> Res<Option<NewStreamType>> {
    match decoded {
        Some(NewStreamType::Control)
        | Some(NewStreamType::Encoder)
        | Some(NewStreamType::Decoder) => Err(Error::HttpClosedCriticalStream),
        Some(NewStreamType::Push) | None => Err(Error::HttpStreamCreation),
        Some(NewStreamType::Http) => Err(Error::HttpFrame),
        Some(NewStreamType::Unknown) => Ok(decoded),
        _ => unreachable!("WebTransportStream is mapped to None at this stage."),
    }
}

fn decode_stream_type(
    stream_type: u64,
    trans_stream_type: StreamType,
) -> Res<Option<NewStreamType>> {
    match (stream_type, trans_stream_type) {
        (HTTP3_UNI_STREAM_TYPE_CONTROL, StreamType::UniDi) => Ok(Some(NewStreamType::Control)),
        (HTTP3_UNI_STREAM_TYPE_PUSH, StreamType::UniDi) => Ok(Some(NewStreamType::Push)),
        (QPACK_UNI_STREAM_TYPE_ENCODER, StreamType::UniDi) => Ok(Some(NewStreamType::Decoder)),
        (QPACK_UNI_STREAM_TYPE_DECODER, StreamType::UniDi) => Ok(Some(NewStreamType::Encoder)),
        (WEBTRANSPORT_UNI_STREAM, StreamType::UniDi) | (WEBTRANSPORT_STREAM, StreamType::BiDi) => {
            Ok(None)
        }
        (H3_FRAME_TYPE_HEADERS, StreamType::BiDi) => Ok(Some(NewStreamType::Http)),
        (_, StreamType::BiDi) => Err(Error::HttpFrame),
        _ => Ok(Some(NewStreamType::Unknown)),
    }
}

impl RecvStream for NewStreamTypeReader {
    fn stream_reset(&mut self, _error: AppError, _reset_type: ResetType) -> Res<()> {
        *self = NewStreamTypeReader::Done;
        Ok(())
    }

    fn receive(&mut self, conn: &mut Connection) -> Res<ReceiveOutput> {
        Ok(self
            .get_type(conn)?
            .map_or(ReceiveOutput::NoOutput, |t| ReceiveOutput::NewStream(t)))
    }

    fn done(&self) -> bool {
        *self == NewStreamTypeReader::Done
    }

    fn stream_type(&self) -> Http3StreamType {
        Http3StreamType::NewStream
    }

    fn http_stream(&mut self) -> Option<&mut dyn HttpRecvStream> {
        None
    }

    fn wt_stream(&mut self) -> Option<&mut dyn WtRecvStream> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{NewStreamTypeReader, HTTP3_UNI_STREAM_TYPE_PUSH};
    use neqo_transport::{Connection, StreamType};
    use std::mem;
    use test_fixture::{connect, now};

    use crate::control_stream_local::HTTP3_UNI_STREAM_TYPE_CONTROL;
    use crate::hframe::H3_FRAME_TYPE_HEADERS;
    use crate::wt_stream::{WEBTRANSPORT_STREAM, WEBTRANSPORT_UNI_STREAM};
    use crate::{NewStreamType, ReceiveOutput, RecvStream, ResetType};
    use neqo_common::Encoder;
    use neqo_qpack::decoder::QPACK_UNI_STREAM_TYPE_DECODER;
    use neqo_qpack::encoder::QPACK_UNI_STREAM_TYPE_ENCODER;

    struct Test {
        conn_c: Connection,
        conn_s: Connection,
        stream_id: u64,
        decoder: NewStreamTypeReader,
    }

    impl Test {
        fn new(stream_type: StreamType) -> Self {
            let (mut conn_c, mut conn_s) = connect();
            // create a stream
            let stream_id = conn_s.stream_create(stream_type).unwrap();
            let out = conn_s.process(None, now());
            mem::drop(conn_c.process(out.dgram(), now()));

            Self {
                conn_c,
                conn_s,
                stream_id,
                decoder: NewStreamTypeReader::new(stream_id),
            }
        }

        fn decode(&mut self, stream_type: &[u64], outcome: ReceiveOutput, done: bool) {
            let mut enc = Encoder::default();
            for i in stream_type {
                enc.encode_varint(*i);
            }
            let len = enc.len() - 1;
            for i in 0..len {
                self.conn_s
                    .stream_send(self.stream_id, &enc[i..i + 1])
                    .unwrap();
                let out = self.conn_s.process(None, now());
                mem::drop(self.conn_c.process(out.dgram(), now()));
                assert_eq!(
                    self.decoder.receive(&mut self.conn_c).unwrap(),
                    ReceiveOutput::NoOutput
                );
                assert_eq!(self.decoder.done(), false);
            }
            self.conn_s
                .stream_send(self.stream_id, &enc[enc.len() - 1..])
                .unwrap();
            let out = self.conn_s.process(None, now());
            mem::drop(self.conn_c.process(out.dgram(), now()));
            assert_eq!(self.decoder.receive(&mut self.conn_c).unwrap(), outcome);
            assert_eq!(self.decoder.done(), done);
        }
    }

    #[test]
    fn decode_streams() {
        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[QPACK_UNI_STREAM_TYPE_DECODER],
            ReceiveOutput::NewStream(NewStreamType::Encoder),
            true,
        );

        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[QPACK_UNI_STREAM_TYPE_ENCODER],
            ReceiveOutput::NewStream(NewStreamType::Decoder),
            true,
        );

        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[HTTP3_UNI_STREAM_TYPE_CONTROL],
            ReceiveOutput::NewStream(NewStreamType::Control),
            true,
        );

        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[HTTP3_UNI_STREAM_TYPE_PUSH],
            ReceiveOutput::NewStream(NewStreamType::Push),
            true,
        );

        let mut t = Test::new(StreamType::BiDi);
        t.decode(
            &[H3_FRAME_TYPE_HEADERS],
            ReceiveOutput::NewStream(NewStreamType::Http),
            true,
        );

        let mut t = Test::new(StreamType::BiDi);
        t.decode(
            &[WEBTRANSPORT_STREAM, 0x1],
            ReceiveOutput::NewStream(NewStreamType::WebTransportStream(1)),
            true,
        );

        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[WEBTRANSPORT_UNI_STREAM, 0x3fff],
            ReceiveOutput::NewStream(NewStreamType::WebTransportStream(16383)),
            true,
        );

        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[0x3fff_ffff_ffff_ffff],
            ReceiveOutput::NewStream(NewStreamType::Unknown),
            true,
        );
    }

    #[test]
    fn done_decoding() {
        let mut t = Test::new(StreamType::UniDi);
        t.decode(
            &[0x3fff],
            ReceiveOutput::NewStream(NewStreamType::Unknown),
            true,
        );
        t.decode(&[HTTP3_UNI_STREAM_TYPE_PUSH], ReceiveOutput::NoOutput, true);
    }

    #[test]
    fn reset() {
        let mut t = Test::new(StreamType::UniDi);
        t.decoder.stream_reset(0x100, ResetType::Remote).unwrap();
        t.decode(&[HTTP3_UNI_STREAM_TYPE_PUSH], ReceiveOutput::NoOutput, true);
    }
}
