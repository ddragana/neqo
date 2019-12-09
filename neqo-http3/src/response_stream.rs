// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::client_events::Http3ClientEvents;
use crate::hframe::{HFrame, HFrameReader};
use crate::{Error, Header, Res};
use neqo_common::{matches, qdebug, qinfo, qtrace};
use neqo_qpack::decoder::QPackDecoder;
use neqo_transport::Connection;
use std::mem;

pub struct PushInfo {
    pub push_id: u64,
    pub header_block: Option<Vec<u8>>,
}

/*
 * Reesponse stream state:
 *    WaitingForResponseHeaders : we wait for headers. in this state we can
 *                                also get a PUSH_PROMISE frame.
 *    DecodingHeaders : In this step the headers will be decoded. The stream
 *                      may be blocked in this state on encoder instructions.
 *    WaitingForData : we got HEADERS, we are waiting for one or more data
 *                     frames. In this state we can receive one or more
 *                     PUSH_PROMIS frames or a HEADERS frame carrying trailers.
 *    ReadingData : we got a DATA frame, now we letting the app read payload.
 *                  From here we will go back to WaitingForData state to wait
 *                  for more data frames or to Closed state
 *    ReadingTrailers : reading trailers.
 *    ClosePending : waiting for app to pick up data, after that we can delete
 *                   the ResponseStream.
 *    Closed: in this state we may delete the ResponseStream.
 */
#[derive(PartialEq, Debug)]
enum ResponseStreamState {
    WaitingForResponseHeaders,
    DecodingHeaders { header_block: Vec<u8>, fin: bool },
    WaitingForData,
    ReadingData { remaining_data_len: usize },
    WaitingForFinAfterTrailers,
    ClosePending, // Close must first be read by application
    Closed,
}

#[derive(Debug, PartialEq)]
enum ResponseHeadersState {
    NoHeaders,
    Ready(Option<Vec<Header>>),
    Read,
}

#[derive(Debug)]
pub(crate) struct ResponseStream {
    state: ResponseStreamState,
    frame_reader: HFrameReader,
    response_headers_state: ResponseHeadersState,
    conn_events: Http3ClientEvents,
    stream_id: u64,
    push_id: Option<u64>,
}

impl ::std::fmt::Display for ResponseStream {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(
            f,
            "ResponseStream stream_id:({} {:?})",
            self.stream_id, self.push_id
        )
    }
}

impl ResponseStream {
    pub fn new(stream_id: u64, push_id: Option<u64>, conn_events: Http3ClientEvents) -> Self {
        ResponseStream {
            state: ResponseStreamState::WaitingForResponseHeaders,
            frame_reader: HFrameReader::new(),
            response_headers_state: ResponseHeadersState::NoHeaders,
            conn_events,
            stream_id,
            push_id,
        }
    }

    fn handle_headers_frame(&mut self, header_block: Vec<u8>, fin: bool) -> Res<()> {
        match self.state {
            ResponseStreamState::WaitingForResponseHeaders => {
                if header_block.is_empty() {
                    self.add_headers(None)?;
                } else {
                    self.state = ResponseStreamState::DecodingHeaders { header_block, fin };
                }
            }
            ResponseStreamState::WaitingForData => {
                // TODO implement trailers, for now just ignore them.
                self.state = ResponseStreamState::WaitingForFinAfterTrailers;
            }
            ResponseStreamState::WaitingForFinAfterTrailers => {
                return Err(Error::HttpFrameUnexpected);
            }
            _ => unreachable!("This functions is only called in WaitingForResponseHeaders | WaitingForData | WaitingForFinAfterTrailers state.")
        }
        Ok(())
    }

    fn handle_data_frame(&mut self, len: u64, fin: bool) -> Res<()> {
        match self.state {
            ResponseStreamState::WaitingForResponseHeaders | ResponseStreamState::WaitingForFinAfterTrailers => {
                return Err(Error::HttpFrameUnexpected);
            }
            ResponseStreamState::WaitingForData => {
                if len > 0 {
                    if fin {
                        return Err(Error::HttpFrameError);
                    }
                    self.state = ResponseStreamState::ReadingData {
                        remaining_data_len: len as usize,
                    };
                }
            }
            _ => unreachable!("This functions is only called in WaitingForResponseHeaders | WaitingForData | WaitingForFinAfterTrailers state.")
        }
        Ok(())
    }

    fn add_headers(&mut self, headers: Option<Vec<Header>>) -> Res<()> {
        if self.response_headers_state != ResponseHeadersState::NoHeaders {
            debug_assert!(
                false,
                "self.response_headers_state must be in state ResponseHeadersState::NoHeaders."
            );
            return Err(Error::HttpInternalError);
        }
        self.response_headers_state = ResponseHeadersState::Ready(headers);
        if let Some(push_id) = self.push_id {
            self.conn_events.push_header_ready(push_id);
        } else {
            self.conn_events.header_ready(self.stream_id);
        }
        self.state = ResponseStreamState::WaitingForData;

        Ok(())
    }

    fn set_state_to_close_pending(&mut self) {
        // Stream has received fin. Depending on headers state set (push_)header_ready
        // or (push_)data_readable event so that app can pick up the fin.
        qdebug!(
            [self],
            "set_state_to_close_pending:  response_headers_state={:?}",
            self.response_headers_state
        );
        match self.response_headers_state {
            ResponseHeadersState::NoHeaders => {
                if let Some(push_id) = self.push_id {
                    self.conn_events.push_header_ready(push_id);
                } else {
                    self.conn_events.header_ready(self.stream_id);
                }
                self.response_headers_state = ResponseHeadersState::Ready(None);
            }
            // In Ready state we are already waiting for app to pick up headers
            // it can also pick up fin, so we do not need a new event.
            ResponseHeadersState::Ready(..) => {}
            ResponseHeadersState::Read => {
                if let Some(push_id) = self.push_id {
                    self.conn_events.push_data_readable(push_id);
                } else {
                    self.conn_events.data_readable(self.stream_id);
                }
            }
        }
        self.state = ResponseStreamState::ClosePending;
    }

    fn recv_frame(&mut self, conn: &mut Connection) -> Res<(Option<HFrame>, bool)> {
        qtrace!([self], "receiving frame header");
        let fin = self.frame_reader.receive(conn, self.stream_id)?;
        if !self.frame_reader.done() {
            Ok((None, fin))
        } else {
            qdebug!([self], "A new frame has been received.");
            Ok((Some(self.frame_reader.get_frame()?), fin))
        }
    }

    pub fn push_id(&self) -> Option<u64> {
        self.push_id
    }

    pub fn read_response_headers(&mut self) -> Res<(Vec<Header>, bool)> {
        if let ResponseHeadersState::Ready(ref mut headers) = self.response_headers_state {
            let mut tmp = Vec::new();
            if let Some(ref mut hdrs) = headers {
                mem::swap(&mut tmp, hdrs);
            }
            self.response_headers_state = ResponseHeadersState::Read;
            let mut fin = false;
            if self.state == ResponseStreamState::ClosePending {
                fin = true;
                self.state = ResponseStreamState::Closed;
            }
            Ok((tmp, fin))
        } else {
            Err(Error::Unavailable)
        }
    }

    pub fn read_response_data(
        &mut self,
        conn: &mut Connection,
        buf: &mut [u8],
    ) -> Res<(usize, bool)> {
        match self.state {
            ResponseStreamState::ReadingData {
                ref mut remaining_data_len,
            } => {
                let to_read = if *remaining_data_len > buf.len() {
                    buf.len()
                } else {
                    *remaining_data_len
                };
                let (amount, fin) = conn.stream_recv(self.stream_id, &mut buf[..to_read])?;
                assert!(amount <= to_read);
                *remaining_data_len -= amount;

                if fin {
                    if *remaining_data_len > 0 {
                        return Err(Error::HttpFrameError);
                    }
                    self.state = ResponseStreamState::Closed;
                } else if *remaining_data_len == 0 {
                    self.state = ResponseStreamState::WaitingForData;
                }

                Ok((amount, fin))
            }
            ResponseStreamState::ClosePending => {
                self.state = ResponseStreamState::Closed;
                Ok((0, true))
            }
            _ => Ok((0, false)),
        }
    }

    pub fn receive(
        &mut self,
        conn: &mut Connection,
        decoder: &mut QPackDecoder,
    ) -> Res<Vec<PushInfo>> {
        let label = if ::log::log_enabled!(::log::Level::Debug) {
            format!("{}", self)
        } else {
            String::new()
        };
        let mut push_promises = Vec::new();
        match loop {
            qdebug!([label], " receive state={:?}.", self.state);
            match self.state {
                // In the following 3 states we nee to read frames.
                ResponseStreamState::WaitingForResponseHeaders
                | ResponseStreamState::WaitingForData
                | ResponseStreamState::WaitingForFinAfterTrailers => {
                    match self.recv_frame(conn)? {
                        (None, true) => {
                            self.set_state_to_close_pending();
                            break Ok(());
                        }
                        (None, false) => break Ok(()),
                        (Some(frame), fin) => {
                            qinfo!(
                                [self],
                                "A new frame has been received: {:?}; state={:?}",
                                frame,
                                self.state
                            );
                            match frame {
                                HFrame::Headers { header_block } => {
                                    self.handle_headers_frame(header_block, fin)?
                                }
                                HFrame::Data { len } => self.handle_data_frame(len, fin)?,
                                HFrame::PushPromise {
                                    push_id,
                                    header_block,
                                } => {
                                    if self.push_id.is_some() {
                                        break Err(Error::HttpFrameUnexpected);
                                    }
                                    push_promises.push(PushInfo {
                                        push_id,
                                        header_block: Some(header_block),
                                    });
                                }
                                HFrame::DuplicatePush { push_id } => {
                                    if self.push_id.is_some() {
                                        break Err(Error::HttpFrameUnexpected);
                                    }
                                    push_promises.push(PushInfo {
                                        push_id,
                                        header_block: None,
                                    });
                                }
                                _ => break Err(Error::HttpFrameUnexpected),
                            }
                            if fin
                                && !matches!(self.state, ResponseStreamState::DecodingHeaders{..})
                            {
                                self.set_state_to_close_pending();
                                break Ok(());
                            }
                        }
                    };
                }
                ResponseStreamState::DecodingHeaders {
                    ref header_block,
                    fin,
                } => match decoder.decode_header_block(header_block, self.stream_id)? {
                    Some(headers) => {
                        self.add_headers(Some(headers))?;
                        if fin {
                            self.set_state_to_close_pending();
                            break Ok(());
                        }
                    }
                    None => {
                        qinfo!([self], "decoding header is blocked.");
                        break Ok(());
                    }
                },
                ResponseStreamState::ReadingData { .. } => {
                    if let Some(push_id) = self.push_id {
                        self.conn_events.push_data_readable(push_id);
                    } else {
                        self.conn_events.data_readable(self.stream_id);
                    }
                    break Ok(());
                }
                ResponseStreamState::ClosePending => {
                    panic!("Stream readable after being closed!");
                }
                ResponseStreamState::Closed => {
                    panic!("Stream readable after being closed!");
                }
            };
        } {
            Ok(()) => Ok(push_promises),
            Err(e) => Err(e),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.state == ResponseStreamState::Closed
    }

    pub fn close(&mut self) {
        self.state = ResponseStreamState::Closed;
    }
}