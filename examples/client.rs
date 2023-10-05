use std::{convert::TryFrom, error::Error};

use ntex_bytes::Bytes;
use ntex_connect as connect;
use ntex_h2::{client, MessageKind};
use ntex_http::{header, uri::Scheme, HeaderMap, Method};
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

    let pool = client::Client::build(
        "127.0.0.1:5928",
        connect::openssl::Connector::new(builder.build()),
    )
    .scheme(Scheme::HTTPS)
    .finish();

    let mut hdrs = HeaderMap::default();
    hdrs.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::try_from("text/plain").unwrap(),
    );
    let (stream, recv_stream) = pool
        .send(Method::GET, "/test/index.html".into(), hdrs, false)
        .await
        .unwrap();

    ntex::rt::spawn(async move {
        while let Some(msg) = recv_stream.recv().await {
            match msg.kind {
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
                    break;
                }
                MessageKind::Disconnect(err) => {
                    println!("Disconnect: {:?}", err);
                    break;
                }
            }
        }
    });

    stream
        .send_payload(Bytes::from_static(b"testing"), true)
        .await
        .unwrap();

    sleep(Seconds(10)).await;
    Ok(())
}
