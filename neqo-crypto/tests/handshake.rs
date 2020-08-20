#![allow(dead_code)]

use neqo_common::qinfo;
use neqo_crypto::{
    AntiReplay, AuthenticationStatus, Client, HandshakeState, RecordList, Res, SecretAgent, Server,
    ZeroRttCheckResult, ZeroRttChecker,
};
use std::mem;
use std::time::Instant;
use test_fixture::{anti_replay, fixture_init, now};

pub fn forward_records(
    now: Instant,
    agent: &mut SecretAgent,
    records_in: RecordList,
) -> Res<RecordList> {
    let mut expected_state = match agent.state() {
        HandshakeState::New => HandshakeState::New,
        _ => HandshakeState::InProgress,
    };
    let mut records_out = RecordList::default();
    for record in records_in {
        assert_eq!(records_out.len(), 0);
        assert_eq!(*agent.state(), expected_state);

        records_out = agent.handshake_raw(now, Some(record))?;
        expected_state = HandshakeState::InProgress;
    }
    Ok(records_out)
}

fn handshake(now: Instant, client: &mut SecretAgent, server: &mut SecretAgent) {
    let mut a = client;
    let mut b = server;
    let mut records = a.handshake_raw(now, None).unwrap();
    let is_done = |agent: &mut SecretAgent| agent.state().is_final();
    while !is_done(b) {
        records = if let Ok(r) = forward_records(now, &mut b, records) {
            r
        } else {
            // TODO(mt) take the alert generated by the failed handshake
            // and allow it to be sent to the peer.
            return;
        };

        if *b.state() == HandshakeState::AuthenticationPending {
            b.authenticated(AuthenticationStatus::Ok);
            records = b.handshake_raw(now, None).unwrap();
        }
        mem::swap(&mut a, &mut b);
    }
}

pub fn connect_at(now: Instant, client: &mut SecretAgent, server: &mut SecretAgent) {
    handshake(now, client, server);
    qinfo!("client: {:?}", client.state());
    qinfo!("server: {:?}", server.state());
    assert!(client.state().is_connected());
    assert!(server.state().is_connected());
}

pub fn connect(client: &mut SecretAgent, server: &mut SecretAgent) {
    connect_at(now(), client, server);
}

pub fn connect_fail(client: &mut SecretAgent, server: &mut SecretAgent) {
    handshake(now(), client, server);
    assert!(!client.state().is_connected());
    assert!(!server.state().is_connected());
}

#[derive(Clone, Copy, Debug)]
pub enum Resumption {
    WithoutZeroRtt,
    WithZeroRtt,
}

pub const ZERO_RTT_TOKEN_DATA: &[u8] = b"zero-rtt-token";

#[derive(Debug)]
pub struct PermissiveZeroRttChecker {
    resuming: bool,
}

impl Default for PermissiveZeroRttChecker {
    fn default() -> Self {
        Self { resuming: true }
    }
}

impl ZeroRttChecker for PermissiveZeroRttChecker {
    fn check(&self, token: &[u8]) -> ZeroRttCheckResult {
        if self.resuming {
            assert_eq!(ZERO_RTT_TOKEN_DATA, token);
        } else {
            assert!(token.is_empty());
        }
        ZeroRttCheckResult::Accept
    }
}

fn zero_rtt_setup(
    mode: Resumption,
    client: &mut Client,
    server: &mut Server,
) -> Option<AntiReplay> {
    if let Resumption::WithZeroRtt = mode {
        client.enable_0rtt().expect("should enable 0-RTT on client");

        let anti_replay = anti_replay();
        server
            .enable_0rtt(
                &anti_replay,
                0xffff_ffff,
                Box::new(PermissiveZeroRttChecker { resuming: false }),
            )
            .expect("should enable 0-RTT on server");
        Some(anti_replay)
    } else {
        None
    }
}

pub fn resumption_setup(mode: Resumption) -> (Option<AntiReplay>, Vec<u8>) {
    fixture_init();

    let mut client = Client::new("server.example").expect("should create client");
    let mut server = Server::new(&["key"]).expect("should create server");
    let anti_replay = zero_rtt_setup(mode, &mut client, &mut server);

    connect(&mut client, &mut server);

    assert!(!client.info().unwrap().resumed());
    assert!(!server.info().unwrap().resumed());
    assert!(!client.info().unwrap().early_data_accepted());
    assert!(!server.info().unwrap().early_data_accepted());

    let server_records = server
        .send_ticket(now(), ZERO_RTT_TOKEN_DATA)
        .expect("ticket sent");
    assert_eq!(server_records.len(), 1);
    let client_records = client
        .handshake_raw(now(), server_records.into_iter().next())
        .expect("records ingested");
    assert_eq!(client_records.len(), 0);

    // `client` is about to go out of scope,
    // but we only need to keep the resumption token, so clone it.
    let token = client.resumption_token().expect("token is present").token;
    (anti_replay, token)
}
