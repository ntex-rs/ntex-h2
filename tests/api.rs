use ntex_error::ErrorDiagnostic;
use ntex_h2::control::ExpectResult;
use ntex_h2::frame::{FrameError, Reason, StreamId};
use ntex_h2::{
    ConnectionError, Control, EncoderError, Message, MessageKind, OperationError, StreamError,
    client::SimpleClient, server::Server,
};
use ntex_http::header::{HeaderName, HeaderValue};
use ntex_http::{HeaderMap, Method, StatusCode, uri::Scheme};
use ntex_io::{Io, testing::IoTest};
use ntex_service::cfg::SharedCfg;
use ntex_util::{channel::mpsc, spawn};

#[test]
fn connection_error_diagnostics_and_goaway() {
    let cases = [
        (
            ConnectionError::GoAway(Reason::CANCEL),
            "h2-conn-GoAway",
            Reason::CANCEL,
        ),
        (
            ConnectionError::UnknownStream("DATA"),
            "h2-conn-UnknownStream",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::Encoder(EncoderError::MaxSizeExceeded),
            "h2-conn-Encoder",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::Decoder(FrameError::BadFrameSize),
            "h2-conn-Decoder",
            Reason::FRAME_SIZE_ERROR,
        ),
        (
            ConnectionError::StreamClosed(StreamId::from(1), "DATA"),
            "h2-conn-StreamClosed",
            Reason::STREAM_CLOSED,
        ),
        (
            ConnectionError::InvalidStreamId("test"),
            "h2-conn-InvalidStreamId",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::UnexpectedSettingsAck,
            "h2-conn-UnexpectedSettingsAck",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::MissingSettings,
            "h2-conn-MissingSettings",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::UnexpectedEnablePush,
            "h2-conn-UnexpectedEnablePush",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::ZeroWindowUpdateValue,
            "h2-conn-ZeroWindowUpdateValue",
            Reason::PROTOCOL_ERROR,
        ),
        (
            ConnectionError::WindowValueOverflow,
            "h2-conn-WindowValueOverflow",
            Reason::FLOW_CONTROL_ERROR,
        ),
        (
            ConnectionError::RecvWindowExceeded,
            "h2-conn-RecvWindowExceeded",
            Reason::FLOW_CONTROL_ERROR,
        ),
        (
            ConnectionError::StreamResetsLimit,
            "h2-conn-StreamResetsLimit",
            Reason::ENHANCE_YOUR_CALM,
        ),
        (
            ConnectionError::KeepaliveTimeout,
            "h2-conn-KeepaliveTimeout",
            Reason::NO_ERROR,
        ),
        (
            ConnectionError::SettingsTimeout,
            "h2-conn-SettingsTimeout",
            Reason::SETTINGS_TIMEOUT,
        ),
        (
            ConnectionError::ReadTimeout,
            "h2-conn-ReadTimeout",
            Reason::NO_ERROR,
        ),
        (
            ConnectionError::WriteTimeout,
            "h2-conn-WriteTimeout",
            Reason::NO_ERROR,
        ),
    ];

    for (err, signature, reason) in cases {
        assert_eq!(err.signature(), signature);
        assert_eq!(err.to_goaway().reason(), reason);
    }
}

#[test]
fn stream_and_operation_error_diagnostics() {
    let stream_errors = [
        (StreamError::Idle("DATA"), "h2-stream-Idle"),
        (StreamError::Closed, "h2-stream-Closed"),
        (StreamError::WindowOverflowed, "h2-stream-WindowOverflowed"),
        (StreamError::RecvWindowExceeded, "h2-stream-RecvWindowExceeded"),
        (
            StreamError::WindowZeroUpdateValue,
            "h2-stream-WindowZeroUpdateValue",
        ),
        (StreamError::TrailersWithoutEos, "h2-stream-TrailersWithoutEos"),
        (
            StreamError::InvalidContentLength,
            "h2-stream-InvalidContentLength",
        ),
        (StreamError::WrongPayloadLength, "h2-stream-WrongPayloadLength"),
        (StreamError::NonEmptyPayload, "h2-stream-NonEmptyPayload"),
        (StreamError::CapacityTimeout, "h2-stream-CapacityTimeout"),
        (StreamError::Reset(Reason::CANCEL), "h2-stream-Reset"),
        (StreamError::LocalReset(Reason::CANCEL), "h2-stream-LocalReset"),
        (
            StreamError::InvalidFrame(FrameError::BadFrameSize),
            "h2-stream-InvalidFrame",
        ),
        (StreamError::MissingPseudo(":method"), "h2-stream-MissingPseudo"),
        (
            StreamError::UnexpectedPseudo(":status"),
            "h2-stream-UnexpectedPseudo",
        ),
        (
            StreamError::InvalidInformational,
            "h2-stream-InvalidInformational",
        ),
    ];
    for (err, signature) in stream_errors {
        assert_eq!(err.signature(), signature);
    }

    let operation_errors = [
        (OperationError::Stream(StreamError::Closed), "h2-stream-Closed"),
        (
            OperationError::Connection(ConnectionError::MissingSettings),
            "h2-conn-MissingSettings",
        ),
        (OperationError::Idle, "h2-oper-Idle"),
        (OperationError::Payload, "h2-oper-Payload"),
        (OperationError::Closed(None), "h2-oper-Closed"),
        (OperationError::RemoteReset(Reason::CANCEL), "h2-oper-RemoteReset"),
        (OperationError::LocalReset(Reason::CANCEL), "h2-oper-LocalReset"),
        (OperationError::OverflowedStreamId, "h2-oper-OverflowedStreamId"),
        (
            OperationError::HeaderListTooLarge { size: 10, max: 5 },
            "h2-oper-HeaderListTooLarge",
        ),
        (OperationError::Disconnecting, "h2-oper-Disconnecting"),
        (OperationError::Disconnected, "h2-oper-Disconnected"),
    ];
    for (err, signature) in operation_errors {
        assert_eq!(err.signature(), signature);
    }
}

#[test]
fn protocol_control_error_accessors() {
    let err = ntex_error::Error::from(ConnectionError::MissingSettings);
    let control = ntex_h2::control::ConnectionError::new(err);
    assert_eq!(control.get_ref().signature(), "h2-conn-MissingSettings");
    assert!(
        control
            .reason(Reason::INTERNAL_ERROR)
            .ack()
            .into_expect()
            .is_none()
    );
}

#[ntex::test]
async fn expect_control_and_message_accessors() {
    let (client_io, server_io) = IoTest::create();
    client_io.remote_buffer_cap(1024 * 1024);
    server_io.remote_buffer_cap(1024 * 1024);

    let (tx, messages) = mpsc::channel();
    spawn(async move {
        let _ = Server::new(async move |msg: Message| {
            let _ = tx.send(msg);
            Ok::<_, ()>(())
        })
        .run(Io::new(server_io, SharedCfg::default()))
        .await;
    });
    let client = SimpleClient::new(
        Io::new(client_io, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    );

    let (_send, _recv) = client
        .send(Method::POST, "/upload".into(), HeaderMap::new(), true)
        .await
        .unwrap();
    let msg = messages.recv().await.unwrap();
    let id = msg.id();
    assert_eq!(msg.stream().id(), id);
    assert!(matches!(msg.kind(), MessageKind::Headers { .. }));

    let Message { stream, kind } = msg;
    let MessageKind::Headers { pseudo, headers, eof } = kind else {
        panic!("unexpected message kind")
    };
    assert!(eof);

    let Control::Expect(mut expect) = Control::<()>::expect(stream, pseudo, headers) else {
        panic!("unexpected control event")
    };
    assert_eq!(expect.stream().id(), id);
    assert_eq!(expect.pseudo().path.as_deref(), Some("/upload"));
    assert!(expect.headers().is_empty());

    expect.headers_mut().insert(
        HeaderName::from_static("x-test"),
        HeaderValue::from_static("value"),
    );
    let (stream, pseudo, headers) = expect.clone().into_parts();
    assert_eq!(stream.id(), id);
    assert_eq!(pseudo.path.as_deref(), Some("/upload"));
    assert_eq!(headers.get("x-test").unwrap(), "value");

    let ack = Control::<()>::expect(stream, pseudo, headers).ack();
    let Some(ExpectResult::Continue(expect)) = ack.into_expect() else {
        panic!("expectation was not accepted")
    };

    let response_headers = HeaderMap::new();
    let ack = expect.fail(StatusCode::EXPECTATION_FAILED, response_headers);
    let Some(ExpectResult::Failed(expect, status, headers)) = ack.into_expect() else {
        panic!("expectation was not rejected")
    };
    assert_eq!(expect.stream().id(), id);
    assert_eq!(status, StatusCode::EXPECTATION_FAILED);
    assert!(headers.is_empty());
}
