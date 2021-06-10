// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::connection::{Http3Connection, Http3State};
use crate::hframe::HFrame;
use crate::recv_message::{MessageType, RecvMessage};
use crate::send_message::SendMessage;
use crate::server_connection_events::{Http3ServerConnEvent, Http3ServerConnEvents};
use crate::wt::WebTransportSession;
use crate::{Error, Header, Http3StreamType, NewStreamType, ReceiveOutput, Res};
use neqo_common::{event::Provider, qdebug, qinfo, qtrace};
use neqo_qpack::QpackSettings;
use neqo_transport::{AppError, Connection, ConnectionEvent, StreamType};
use std::rc::Rc;
use std::time::Instant;

#[derive(Debug)]
pub struct Http3ServerHandler {
    base_handler: Http3Connection,
    events: Http3ServerConnEvents,
    needs_processing: bool,
}

impl ::std::fmt::Display for Http3ServerHandler {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Http3 server connection")
    }
}

impl Http3ServerHandler {
    pub(crate) fn new(qpack_settings: QpackSettings, enable_wt: bool) -> Self {
        Self {
            base_handler: Http3Connection::new(qpack_settings, enable_wt),
            events: Http3ServerConnEvents::default(),
            needs_processing: false,
        }
    }

    /// Supply a response for a request.
    pub(crate) fn set_response(
        &mut self,
        stream_id: u64,
        headers: &[Header],
        data: &[u8],
    ) -> Res<()> {
        self.base_handler
            .send_streams
            .get_mut(&stream_id)
            .ok_or(Error::InvalidStreamId)?
            .http_stream()
            .ok_or(Error::InvalidStreamId)?
            .set_message(headers, Some(data))?;
        self.base_handler
            .insert_streams_have_data_to_send(stream_id);
        Ok(())
    }

    /// Accept a WebTransport Session request
    pub(crate) fn wt_session_response(
        &mut self,
        conn: &mut Connection,
        stream_id: u64,
        accept: bool,
    ) -> Res<()> {
        let recv_stream = self.base_handler.recv_streams.get_mut(&stream_id);
        if let Some(r) = &recv_stream {
            if r.stream_type() != Http3StreamType::WebTransportSession {
                return Err(Error::InvalidStreamId);
            }
        }

        let send_stream = self.base_handler.send_streams.remove(&stream_id);
        let recv_stream = self.base_handler.recv_streams.remove(&stream_id);
        match (send_stream, recv_stream) {
            (None, None) => Err(Error::InvalidStreamId),
            (None, Some(_)) | (Some(_), None) => {
                // TODO this needs a better error
                self.base_handler.stream_reset(
                    conn,
                    stream_id,
                    Error::HttpRequestRejected.code(),
                )?;

                Err(Error::InvalidStreamId)
            }
            (Some(mut s), Some(_)) => {
                // TODO check that streams are not closed.
                let headers = if accept {
                    vec![(":status".into(), "200".into())]
                } else {
                    vec![(":status".into(), "400".into())]
                };
                s.http_stream()
                    .ok_or(Error::InvalidStreamId)?
                    .set_message(&headers, None)?;
                WebTransportSession::create_session_server(
                    stream_id,
                    s,
                    Box::new(self.events.clone()),
                    &mut self.base_handler,
                );
                self.base_handler
                    .insert_streams_have_data_to_send(stream_id);
                Ok(())
            }
        }
    }

    /// Create a WebTraansport stream.
    pub fn wt_create_stream(
        &mut self,
        conn: &mut Connection,
        stream_id: u64,
        stream_type: StreamType,
    ) -> Res<u64> {
        let wt_session = self
            .base_handler
            .send_streams
            .get_mut(&stream_id)
            .ok_or(Error::InvalidStreamId)?
            .get_wt_session()
            .ok_or(Error::InvalidStreamId)?;
        match WebTransportSession::create_new_stream_local(wt_session, stream_type, conn)? {
            (id, s, Some(r)) => {
                self.base_handler.add_streams(id, s, r);
                Ok(id)
            }
            (id, s, None) => {
                let _ = self.base_handler.send_streams.insert(id, s);
                Ok(id)
            }
        }
    }

    // Send data on a WebTransport stream
    pub fn wt_stream_send_data(
        &mut self,
        conn: &mut Connection,
        stream_id: u64,
        data: &[u8],
    ) -> Res<usize> {
        let wt_stream = self
            .base_handler
            .send_streams
            .get_mut(&stream_id)
            .ok_or(Error::InvalidStreamId)?
            .wt_stream()
            .ok_or(Error::InvalidStreamId)?;
        let res = wt_stream.send_data(conn, data)?;
        if res > 0 {
            self.needs_processing = true;
        }
        Ok(res)
    }

    /// Reset a request.
    pub fn stream_reset(
        &mut self,
        conn: &mut Connection,
        stream_id: u64,
        app_error: AppError,
    ) -> Res<()> {
        self.base_handler.stream_reset(conn, stream_id, app_error)?;
        self.events.remove_events_for_stream_id(stream_id);
        self.needs_processing = true;
        Ok(())
    }

    /// Process HTTTP3 layer.
    pub fn process_http3(&mut self, conn: &mut Connection, now: Instant) {
        qtrace!([self], "Process http3 internal.");
        if matches!(self.base_handler.state(), Http3State::Closed(..)) {
            return;
        }

        let res = self.check_connection_events(conn, now);
        if !self.check_result(conn, now, &res) && self.base_handler.state().active() {
            let res = self.base_handler.process_sending(conn);
            self.check_result(conn, now, &res);
        }
    }

    /// Take the next available event.
    pub(crate) fn next_event(&mut self) -> Option<Http3ServerConnEvent> {
        self.events.next_event()
    }

    /// Whether this connection has events to process or data to send.
    pub(crate) fn should_be_processed(&mut self) -> bool {
        if self.needs_processing {
            self.needs_processing = false;
            return true;
        }
        self.base_handler.has_data_to_send() | self.events.has_events()
    }

    // This function takes the provided result and check for an error.
    // An error results in closing the connection.
    fn check_result<ERR>(&mut self, conn: &mut Connection, now: Instant, res: &Res<ERR>) -> bool {
        match &res {
            Err(e) => {
                self.close(conn, now, e);
                true
            }
            _ => false,
        }
    }

    fn close(&mut self, conn: &mut Connection, now: Instant, err: &Error) {
        qinfo!([self], "Connection error: {}.", err);
        conn.close(now, err.code(), &format!("{}", err));
        self.base_handler.close(err.code());
        self.events
            .connection_state_change(self.base_handler.state());
    }

    // If this return an error the connection must be closed.
    fn check_connection_events(&mut self, conn: &mut Connection, now: Instant) -> Res<()> {
        qtrace!([self], "Check connection events.");
        while let Some(e) = conn.next_event() {
            qdebug!([self], "check_connection_events - event {:?}.", e);
            match e {
                ConnectionEvent::NewStream { stream_id } => {
                    self.base_handler.add_new_stream(stream_id.as_u64())
                }
                ConnectionEvent::RecvStreamReadable { stream_id } => {
                    self.handle_stream_readable(conn, stream_id)?;
                }
                ConnectionEvent::RecvStreamReset {
                    stream_id,
                    app_error,
                } => {
                    self.base_handler
                        .handle_stream_reset(stream_id, app_error)?;
                }
                ConnectionEvent::SendStreamStopSending {
                    stream_id,
                    app_error,
                } => self
                    .base_handler
                    .handle_stream_stop_sending(stream_id, app_error)?,
                ConnectionEvent::StateChange(state) => {
                    if self.base_handler.handle_state_change(conn, &state)? {
                        if self.base_handler.state() == Http3State::Connected {
                            let settings = self.base_handler.save_settings();
                            conn.send_ticket(now, &settings)?;
                        }
                        self.events
                            .connection_state_change(self.base_handler.state());
                    }
                }
                ConnectionEvent::AuthenticationNeeded
                | ConnectionEvent::EchFallbackAuthenticationNeeded { .. }
                | ConnectionEvent::ZeroRttRejected
                | ConnectionEvent::ResumptionToken(..) => return Err(Error::HttpInternal(4)),
                ConnectionEvent::SendStreamWritable { .. }
                | ConnectionEvent::SendStreamComplete { .. }
                | ConnectionEvent::SendStreamCreatable { .. } => {}
            }
        }
        Ok(())
    }

    fn handle_stream_readable(&mut self, conn: &mut Connection, stream_id: u64) -> Res<()> {
        match self.base_handler.handle_stream_readable(conn, stream_id)? {
            ReceiveOutput::NewStream(NewStreamType::Push) => Err(Error::HttpStreamCreation),
            ReceiveOutput::NewStream(NewStreamType::Http) => {
                self.base_handler.add_streams(
                    stream_id,
                    Box::new(SendMessage::new(
                        stream_id,
                        self.base_handler.qpack_encoder.clone(),
                        Box::new(self.events.clone()),
                    )),
                    Box::new(RecvMessage::new(
                        MessageType::Request,
                        stream_id,
                        Rc::clone(&self.base_handler.qpack_decoder),
                        Box::new(self.events.clone()),
                        None,
                        true,
                    )),
                );
                let res = self.base_handler.handle_stream_readable(conn, stream_id)?;
                assert_eq!(ReceiveOutput::NoOutput, res);
                Ok(())
            }
            ReceiveOutput::ControlFrames(control_frames) => {
                for f in control_frames {
                    match f {
                        HFrame::MaxPushId { .. } | HFrame::Settings { .. } => {
                            // Settings frame is only handled by the client.
                            // TODO implement push
                            Ok(())
                        }
                        HFrame::Goaway { .. } | HFrame::CancelPush { .. } => {
                            Err(Error::HttpFrameUnexpected)
                        }
                        _ => unreachable!(
                            "we should only put MaxPushId and Goaway into control_frames."
                        ),
                    }?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Response data are read directly into a buffer supplied as a parameter of this function to avoid copying
    /// data.
    /// # Errors
    /// It returns an error if a stream does not exist or an error happen while reading a stream, e.g.
    /// early close, protocol error, etc.
    pub fn read_request_data(
        &mut self,
        conn: &mut Connection,
        now: Instant,
        stream_id: u64,
        buf: &mut [u8],
        wt: bool,
    ) -> Res<(usize, bool)> {
        qinfo!([self], "read_data from stream {}.", stream_id);
        let (res, recv_stream) = if !wt {
            let recv_stream = self
                .base_handler
                .recv_streams
                .get_mut(&stream_id)
                .ok_or(Error::InvalidStreamId)?;
            let http = recv_stream.http_stream().ok_or(Error::InvalidStreamId)?;
            (http.read_data(conn, buf), recv_stream)
        } else {
            let recv_stream = self
                .base_handler
                .recv_streams
                .get_mut(&stream_id)
                .ok_or(Error::InvalidStreamId)?;
            let wt = recv_stream.wt_stream().ok_or(Error::InvalidStreamId)?;
            (wt.read_data(conn, buf), recv_stream)
        };
        match res {
            Ok((amount, fin)) => {
                if recv_stream.done() {
                    self.base_handler.recv_streams.remove(&stream_id);
                }
                Ok((amount, fin))
            }
            Err(e) => {
                self.close(conn, now, &e);
                Err(e)
            }
        }
    }
}
