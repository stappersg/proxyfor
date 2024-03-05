mod certificate_authority;
mod rewind;
mod server;

use crate::{certificate_authority::load_ca, server::Server};

use anyhow::{anyhow, Result};
use certificate_authority::CertificateAuthority;
use clap::Parser;
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
};
use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::{net::TcpListener, task::JoinHandle};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let addr: IpAddr = cli
        .bind
        .parse()
        .map_err(|_| anyhow!("Invalid bind '{}'", cli.bind))?;
    let running = Arc::new(AtomicBool::new(true));
    let target = cli.target.map(|url| {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            format!("http://{}", url)
        } else {
            url
        }
    });
    let ca = load_ca()?;
    let handle = serve(addr, cli.port, target, ca, running.clone())?;
    let running = Arc::new(AtomicBool::new(true));
    eprintln!("Listening on {}:{}", cli.bind, cli.port);
    tokio::select! {
        ret = handle => {
            if let Err(e) = ret {
                eprintln!("{}", e);
            }
            Ok(())
        },
        _ = shutdown_signal() => {
            running.store(false, Ordering::SeqCst);
            Ok(())
        },
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Specify bind address or unix socket
    #[clap(short = 'b', long, default_value = "0.0.0.0")]
    pub bind: String,
    /// Specify port to listen on
    #[clap(short = 'p', long, default_value_t = 8088)]
    pub port: u16,
    /// Proxy target
    #[clap(value_name = "URL")]
    pub target: Option<String>,
}

fn serve(
    addr: IpAddr,
    port: u16,
    target: Option<String>,
    ca: CertificateAuthority,
    running: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let server_handle = Arc::new(Server {
        target,
        ca,
        running,
    });
    let listener = create_listener(SocketAddr::new(addr, port))?;
    let handle = tokio::spawn(async move {
        loop {
            let accept = match listener.accept().await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let (cnx, _) = accept;
            let stream = TokioIo::new(cnx);
            tokio::spawn(handle_stream(server_handle.clone(), stream));
        }
    });
    Ok(handle)
}

async fn handle_stream<T>(handle: Arc<Server>, stream: TokioIo<T>)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let hyper_service =
        service_fn(move |request: hyper::Request<Incoming>| handle.clone().handle(request));

    let ret = Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(stream, hyper_service)
        .await;

    if let Err(err) = ret {
        match err.downcast_ref::<std::io::Error>() {
            Some(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
            _ => eprintln!("Serving connection {}", err),
        }
    }
}

fn create_listener(addr: SocketAddr) -> Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024 /* Default backlog */)?;
    let std_listener = StdTcpListener::from(socket);
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    Ok(listener)
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C signal handler")
}