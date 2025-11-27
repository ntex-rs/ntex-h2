use ntex::service::{ServiceFactory, fn_service};
use ntex_h2::{Control, Message, MessageKind, OperationError, server};
use ntex_http::{HeaderMap, StatusCode, header};
use ntex_tls::openssl::SslAcceptor;
use openssl::ssl::{self, AlpnError, SslFiletype, SslMethod};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let _ = env_logger::init();

    // create self-signed certificates using:
    //   openssl req -x509 -nodes -subj '/CN=localhost' -newkey rsa:4096 -keyout examples/key8.pem -out examples/cert.pem -days 365 -keyform PEM
    //   openssl rsa -in examples/key8.pem -out examples/key.pem
    let mut builder = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else {
            Err(AlpnError::NOACK)
        }
    });
    let acceptor = builder.build();

    ntex::server::build()
        .bind("http", "127.0.0.1:5928", move |_| {
            SslAcceptor::new(acceptor.clone())
                .map_err(|_err| server::ServerError::Service(()))
                .and_then(
                    server::Server::new(fn_service(|msg: Message| async move {
                        let Message { stream, kind } = msg;
                        match kind {
                            MessageKind::Headers {
                                pseudo,
                                headers,
                                eof,
                            } => {
                                println!(
                                    "Got request (eof: {}): {:#?}\nheaders: {:#?}",
                                    eof, pseudo, headers
                                );
                                // return Err(());
                                let mut hdrs = HeaderMap::default();
                                hdrs.insert(
                                    header::CONTENT_TYPE,
                                    header::HeaderValue::try_from("text/plain").unwrap(),
                                );
                                stream.send_response(StatusCode::OK, hdrs, false)?;
                                stream.send_payload("hello world".into(), false).await?;

                                let mut hdrs = HeaderMap::default();
                                hdrs.insert(
                                    header::CONTENT_TYPE,
                                    header::HeaderValue::try_from("blah").unwrap(),
                                );
                                stream.send_trailers(hdrs);
                            }
                            MessageKind::Data(data, _cap) => {
                                println!("Got data: {:?}", data.len());
                            }
                            MessageKind::Eof(data) => {
                                println!("Got eof: {:?}", data);
                            }
                            MessageKind::Disconnect(err) => {
                                log::trace!("Disconnect: {:?}", err);
                            }
                        }
                        Ok::<_, OperationError>(())
                    }))
                    .control(|msg: Control<_>| async move {
                        println!("Control message: {:?}", msg);
                        Ok::<_, ()>(msg.ack())
                    }),
                )
        })?
        .workers(1)
        .run()
        .await
}
