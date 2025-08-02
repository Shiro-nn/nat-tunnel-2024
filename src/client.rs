use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use std::{net::SocketAddr, os::unix::io::AsRawFd};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream};
use yamux::{Config as YamuxConfig, Control as YamuxControl, Mode as YamuxMode, Session as YamuxSession};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Public server in the form ip:port
    #[arg(long)]
    server: SocketAddr,

    /// Virtual IP that will be assigned to the local TUN interface (e.g. 10.2.0.2/24)
    #[arg(long, default_value = "10.2.0.2")]
    vip: String,

    /// MTU for the TUN interface
    #[arg(long, default_value_t = 1400)]
    mtu: i32,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    // 1. Create and configure TUN device
    let dev = create_tun(&cli.vip, cli.mtu)?;
    let mut reader = dev.clone();
    let mut writer = dev; // original becomes writer

    // 2. Connect to public server and wrap with Yamux
    let tcp = TcpStream::connect(cli.server).await.context("connect to server")?;
    let mut session = YamuxSession::new(tcp, YamuxConfig::default(), YamuxMode::Client);
    let mut control = session.control();

    // Task: forward packets from TUN -> server
    tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(cli.mtu as usize + 100);
        buf.resize(cli.mtu as usize + 100, 0);
        loop {
            match reader.read(&mut buf).await {
                Ok(n) => {
                    if n == 0 { continue; }
                    if let Ok(mut stream) = control.open_stream().await {
                        if stream.write_all(&buf[..n]).await.is_err() { break; }
                    }
                }
                Err(e) => {
                    log::error!("TUN read error: {e}");
                    break;
                }
            }
        }
    });

    // Task: forward packets from server -> TUN
    let mut buf = vec![0u8; cli.mtu as usize + 100];
    loop {
        match session.next_stream().await {
            Ok(mut stream) => {
                let mut writer_clone = writer.clone();
                let mtu = cli.mtu as usize + 100;
                tokio::spawn(async move {
                    let mut inner_buf = vec![0u8; mtu];
                    loop {
                        match stream.read(&mut inner_buf).await {
                            Ok(n) if n > 0 => {
                                if writer_clone.write_all(&inner_buf[..n]).await.is_err() { break; }
                            }
                            _ => break,
                        }
                    }
                });
            }
            Err(e) => {
                log::error!("yamux error: {e}");
                break;
            }
        }
    }
    Ok(())
}

fn create_tun(ip: &str, mtu: i32) -> Result<tun::TunSocket> {
    use tun::traits::Device as _;
    let mut config = tun::Configuration::default();
    config.address(ip).netmask("255.255.255.0").mtu(mtu).up();
    let dev = tun::create(&config).context("creating TUN device")?;

    // Bring interface up using ip command (Linux only). This could be moved to cross‑platform code.
    let if_name = dev.name();
    std::process::Command::new("ip").args(["link", "set", "up", "dev", &if_name]).status()?;
    Ok(dev)
}
