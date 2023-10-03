#![allow(dead_code, unused_variables)]
use ntex_h2::{client, server, Config, ControlMessage, Message};
use ntex_http::uri::Scheme;
use ntex_io::{testing::IoTest, Io};
use ntex_service::fn_service;
use ntex_util::channel::mpsc;

pub mod frames;
mod utils;

pub use self::utils::*;

pub fn start_client(io: IoTest) -> client::SimpleClient {
    io.remote_buffer_cap(1000000);
    client::SimpleClient::new(
        Io::new(io),
        Config::client(),
        Scheme::HTTP,
        "localhost".into(),
    )
}

pub fn start_server(io: IoTest) -> mpsc::Receiver<Message> {
    io.remote_buffer_cap(1000000);

    let (tx, rx) = mpsc::channel();
    ntex_rt::spawn(async move {
        let _ = server::Server::new(
            Config::server(),
            fn_service(|msg: ControlMessage<()>| async move {
                log::trace!("Control message: {:?}", msg);
                Ok::<_, ()>(msg.ack())
            }),
            fn_service(move |msg: Message| {
                let _ = tx.send(msg);
                async { Ok(()) }
            }),
        )
        .handler()
        .run(Io::new(io).into())
        .await;
    });

    rx
}

#[macro_export]
macro_rules! get_headers {
    ($msg: ident) => {{
        use ntex_h2::MessageKind;

        let mut t = $msg;
        match t.take() {
            MessageKind::Headers {
                pseudo,
                headers,
                eof,
            } => (pseudo, headers, eof),
            _ => panic!("unexpected message kind; actual={:?}", t),
        }
    }};
}
