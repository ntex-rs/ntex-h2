use std::convert::TryFrom;

use ntex::service::fn_service;
use ntex_h2::{server, ControlMessage, Message, MessageKind, OperationError};
use ntex_http::{header, HeaderMap, StatusCode};
use ntex_util::time::{sleep, Millis};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "trace,polling=info,mio=info");
    std::env::set_var("RUST_BACKTRACE", "1");
    env_logger::init();

    ntex::server::Server::build()
        .bind("http", "127.0.0.1:5928", move |_| {
            server::Server::build()
                .configure(|cfg| cfg.max_concurrent_streams(10))
                .control(|msg: ControlMessage<_>| async move {
                    log::trace!("Control message: {:?}", msg);
                    Ok::<_, ()>(msg.ack())
                })
                .finish(fn_service(|mut msg: Message| async move {
                    match msg.kind().take() {
                        MessageKind::Headers {
                            pseudo,
                            headers,
                            eof,
                        } => {
                            log::trace!(
                                "{:?} got request (eof: {}): {:#?}\nheaders: {:#?}",
                                msg.id(),
                                eof,
                                pseudo,
                                headers
                            );
                            // return Err(());
                            let mut hdrs = HeaderMap::default();
                            hdrs.insert(
                                header::CONTENT_TYPE,
                                header::HeaderValue::try_from("text/plain").unwrap(),
                            );
                            msg.stream().send_response(StatusCode::OK, hdrs, false)?;

                            if eof {
                                sleep(Millis(150)).await;
                                log::trace!("Sending payload for {:?}", msg.id(),);
                                msg.stream()
                                    .send_payload("hello world".into(), true)
                                    .await?;
                            }
                        }
                        MessageKind::Data(data, _cap) => {
                            log::trace!("Got data: {:?}", data.len());
                        }
                        MessageKind::Eof(data) => {
                            log::trace!("Got eof: {:?}", data);
                            msg.stream()
                                .send_payload("hello world".into(), true)
                                .await?;
                        }
                        MessageKind::Disconnect(err) => {
                            log::trace!("Disconnect: {:?}", err);
                        }
                        MessageKind::Empty => {}
                    }
                    Ok::<_, OperationError>(())
                }))
        })?
        .workers(1)
        .stop_runtime()
        .run()
        .await
}
