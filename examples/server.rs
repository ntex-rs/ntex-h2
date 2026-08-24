use std::{error::Error as StdError, rc::Rc};

use ntex::service::{cfg::SharedCfg, fn_service};
use ntex_error::Error;
use ntex_h2::{Control, Message, MessageKind, OperationError, ServiceConfig, server};
use ntex_http::{HeaderMap, StatusCode, header};
use ntex_util::time::{Millis, sleep};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let _ = env_logger::try_init();

    let cfg = SharedCfg::new("SRV").add(ServiceConfig::new().set_max_concurrent_streams(10));

    ntex::server::build()
        .bind("http", "127.0.0.1:5928", cfg, async move |_| {
            server::Server::<_, ()>::new(fn_service(async move |msg: Message| {
                let Message { stream, kind } = msg;
                match kind {
                    MessageKind::Headers { pseudo, headers, eof } => {
                        log::trace!(
                            "{:?} got request (eof: {}): {:#?}\nheaders: {:#?}",
                            stream.id(),
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
                        stream.send_response(StatusCode::OK, hdrs, false)?;

                        if eof {
                            sleep(Millis(150)).await;
                            log::trace!("Sending payload for {:?}", stream.id(),);
                            stream.send_payload("hello world", true).await?;
                        }
                    }
                    MessageKind::Data(data, _cap) => {
                        log::trace!("Got data: {:?}", data.len());
                    }
                    MessageKind::Eof(data) => {
                        log::trace!("Got eof: {:?}", data);
                        stream.send_payload("hello world", true).await?;
                    }
                    MessageKind::Disconnect(err) => {
                        log::trace!("Disconnect: {:?}", err);
                    }
                }
                Ok::<_, Error<OperationError>>(())
            }))
            .control(|msg: Control<_>| async move {
                log::trace!("Control message: {:?}", msg);
                Ok::<_, Rc<dyn StdError>>(msg.ack())
            })
        })?
        .workers(1)
        .stop_runtime()
        .run()
        .await
}
