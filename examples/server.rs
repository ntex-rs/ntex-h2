use ntex::service::{fn_service, pipeline_factory};
use ntex_h2::server;
use ntex_tls::openssl::Acceptor;
use openssl::ssl::{AlpnError, SslAcceptor, SslFiletype, SslMethod};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "trace,polling=info,mio=info");
    env_logger::init();

    // create self-signed certificates using:
    //   openssl req -x509 -nodes -subj '/CN=localhost' -newkey rsa:4096 -keyout examples/key8.pem -out examples/cert.pem -days 365 -keyform PEM
    //   openssl rsa -in examples/key8.pem -out examples/key.pem
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
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

    ntex::server::Server::build()
        .bind("http", "127.0.0.1:8443", move |_| {
            pipeline_factory(Acceptor::new(acceptor.clone()))
                .map_err(|_err| server::ServerError::Service(()))
                .and_then(
                    server::Server::build()
                        .control(|msg: server::ControlFrame| async move {
                            println!("T: {:?}", msg);
                            Ok::<_, ()>(())
                        })
                        .finish(fn_service(|_msg: ()| async move { Ok::<_, ()>(()) })),
                )
        })?
        .workers(1)
        .run()
        .await
}
