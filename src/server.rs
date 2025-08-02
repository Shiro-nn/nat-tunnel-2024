use anyhow::{Context, Result};
use clap::Parser;
use std::{net::{Ipv4Addr, SocketAddr}, sync::Arc};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::Mutex};
use yamux::{Config as YamuxConfig, Mode as YamuxMode, Session as YamuxSession};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Listen address for clients (control plane)
    #[arg(long, default_value = "0.0.0.0:7000")]
    bind: SocketAddr,

    /// Public port that should be exposed (e.g. 8080)
    #[arg(long, default_value_t = 8080)]
    public_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    // 1. Accept client
    let listener = TcpListener::bind(cli.bind).await?;
    let (tcp, _) = listener.accept().await.context("waiting for client")?;

    // 2. Wrap with Yamux session
    let mut session = YamuxSession::new(tcp, YamuxConfig::default(), YamuxMode::Server);
    let control = session.control();

    // 3. Accept public connections and forward to client via Yamux
    let public_listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, cli.public_port)).await?;
    let control = Arc::new(Mutex::new(control));

    // Task: handle new connections from internet
    let ctrl_clone = control.clone();
    tokio::spawn(async move {
        loop {
            match public_listener.accept().await {
                Ok((mut pub_stream, remote_addr)) => {
                    log::info!("incoming connection from {remote_addr}");
                    let mut ctrl = ctrl_clone.lock().await;
                    if let Ok(mut yamux_stream) = ctrl.open_stream().await {
                        // Bi‑directional copy
                        tokio::spawn(async move {
                            let (mut r1, mut w1) = pub_stream.split();
                            let (mut r2, mut w2) = yamux_stream.split();
                            let a = tokio::io::copy(&mut r1, &mut w2);
                            let b = tokio::io::copy(&mut r2, &mut w1);
                            let _ = tokio::join!(a, b);
                        });
                    }
                }
                Err(e) => {
                    log::error!("public listener error: {e}");
                    break;
                }
            }
        }
    });

    // 4. Drain client‑>server streams and forward to public connections
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match session.next_stream().await {
            Ok(mut stream) => {
                // For simplicity, discard data or implement reverse copy if needed
                while stream.read(&mut buf).await? > 0 {}
            }
            Err(e) => {
                log::error!("yamux server error: {e}");
                break;
            }
        }
    }

    Ok(())
}