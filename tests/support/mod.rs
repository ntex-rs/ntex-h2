use std::{error::Error, rc::Rc};

use ntex_h2::{Control, Message, client, server};
use ntex_http::uri::Scheme;
use ntex_io::{Io, testing::IoTest};
use ntex_service::cfg::SharedCfg;
use ntex_util::channel::mpsc;

pub mod frames;
mod utils;

pub use self::utils::*;

pub fn start_client(io: IoTest) -> client::SimpleClient {
    io.remote_buffer_cap(1000000);
    client::SimpleClient::new(
        Io::new(io, SharedCfg::default()),
        Scheme::HTTP,
        "localhost".into(),
    )
}

pub fn start_server(io: IoTest) -> mpsc::Receiver<Message> {
    io.remote_buffer_cap(1000000);

    let (tx, rx) = mpsc::channel();
    ntex_util::spawn(async move {
        let _ = server::Server::new(async move |msg: Message| {
            let _ = tx.send(msg);
            Ok::<_, ()>(())
        })
        .control(async move |msg: Control<()>| {
            log::trace!("Control message: {:?}", msg);
            Ok::<_, Rc<dyn Error>>(msg.ack())
        })
        .run(Io::new(io, SharedCfg::default()))
        .await;
    });

    rx
}

#[macro_export]
macro_rules! get_headers {
    ($msg: ident) => {{
        use ntex_h2::MessageKind;

        match $msg.kind {
            MessageKind::Headers { pseudo, headers, eof } => (pseudo, headers, eof),
            _ => panic!("unexpected message kind; actual={:?}", $msg),
        }
    }};
}
