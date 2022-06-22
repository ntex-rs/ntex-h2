use std::{convert::TryFrom, error::Error};

use ntex_bytes::Bytes;
use ntex_connect as connect;
use ntex_h2::{client, Message, MessageKind};
use ntex_http::{header, HeaderMap, Method};
use ntex_service::fn_service;
use ntex_util::time::{sleep, Seconds};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

#[ntex::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    std::env::set_var("RUST_LOG", "trace,polling=info,mio=info");
    env_logger::init();

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let _ = builder
        .set_alpn_protos(b"\x02h2\x08http/1.1")
        .map_err(|e| log::error!("Cannot set alpn protocol: {:?}", e));

    let connector =
        client::Connector::new().connector(connect::openssl::Connector::new(builder.build()));

    let connection = connector.connect("127.0.0.1:5928").await.unwrap();

    let client = connection.client();
    ntex::rt::spawn(async move {
        let _ = connection
            .start(fn_service(|mut msg: Message| async move {
                match msg.kind().take() {
                    MessageKind::Headers {
                        pseudo,
                        headers,
                        eof,
                    } => {
                        println!(
                            "Got response (eof: {}): {:#?}\nheaders: {:#?}",
                            eof, pseudo, headers
                        );
                    }
                    MessageKind::Data(data, _cap) => {
                        println!("Got data: {:?}", data);
                    }
                    MessageKind::Eof(data) => {
                        println!("Got eof: {:?}", data);
                    }
                    MessageKind::Empty => {}
                }
                Ok::<_, ()>(())
            }))
            .await;
    });

    let mut hdrs = HeaderMap::default();
    hdrs.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::try_from("text/plain").unwrap(),
    );
    let stream = client
        .send_request(Method::GET, "/test/index.html".into(), hdrs)
        .await
        .unwrap();
    stream
        .send_payload(Bytes::from_static(b"testing"), true)
        .await
        .unwrap();

    sleep(Seconds(10)).await;
    Ok(())
}
