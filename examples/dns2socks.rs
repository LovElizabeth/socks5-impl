mod util;

use moka::future::Cache;
use socks5_impl::{
    client,
    protocol::{Address, UserKey},
    Error, Result,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufStream},
    net::{TcpListener, TcpStream, ToSocketAddrs, UdpSocket},
};
use trust_dns_proto::op::{Message, Query};
use util::dns;

const MAX_BUFFER_SIZE: usize = 4096;

/// Proxy server to routing DNS query to SOCKS5 server
#[derive(clap::Parser, Debug, Clone, PartialEq, Eq)]
#[command(author, version, about = "Proxy server to routing DNS query to SOCKS5 server", long_about = None)]
pub struct CmdOpt {
    /// Listen address
    #[clap(short, long, value_name = "address:port", default_value = "0.0.0.0:53")]
    listen_addr: SocketAddr,

    /// Remote DNS server address
    #[clap(short, long, value_name = "address:port", default_value = "8.8.8.8:53")]
    dns_remote_server: SocketAddr,

    /// SOCKS5 proxy server address
    #[clap(short, long, value_name = "address:port", default_value = "127.0.0.1:1080")]
    socks5_server: SocketAddr,

    /// User name for SOCKS5 authentication
    #[clap(short, long, value_name = "user name")]
    username: Option<String>,

    /// Password for SOCKS5 authentication
    #[clap(short, long, value_name = "password")]
    password: Option<String>,

    /// Force to use TCP to proxy DNS query
    #[clap(short, long)]
    force_tcp: bool,

    /// Cache DNS query records
    #[clap(short, long)]
    cache_records: bool,

    /// Verbosity level
    #[arg(short, long, value_name = "level", value_enum, default_value = "info")]
    verbosity: ArgVerbosity,

    /// Timeout for DNS query
    #[clap(short, long, value_name = "seconds", default_value = "5")]
    timeout: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum ArgVerbosity {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt: CmdOpt = clap::Parser::parse();

    dotenvy::dotenv().ok();

    let default = format!("{}={:?}", module_path!(), opt.verbosity);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default)).init();

    let user_key = match (&opt.username, &opt.password) {
        (Some(username), password) => Some(UserKey::new(username, password.clone().unwrap_or_default())),
        _ => None,
    };

    let timeout = Duration::from_secs(opt.timeout);

    let cache = create_dns_cache();

    tokio::select! {
        res = tokio::spawn(udp_thread(opt.clone(), user_key.clone(), cache.clone(), timeout)) => {
            match res {
                Ok(Err(e)) => log::error!("UDP error \"{}\"", e),
                Err(e) => log::error!("UDP error \"{}\"", e),
                _ => {}
            }
        },
        res = tokio::spawn(tcp_thread(opt, user_key, cache, timeout)) => {
            match res {
                Ok(Err(e)) => log::error!("TCP error \"{}\"", e),
                Err(e) => log::error!("TCP error \"{}\"", e),
                _ => {}
            }
        },
    }

    Ok(())
}

async fn udp_thread(opt: CmdOpt, user_key: Option<UserKey>, cache: Cache<Vec<Query>, Message>, timeout: Duration) -> Result<()> {
    let listener = Arc::new(UdpSocket::bind(&opt.listen_addr).await?);
    log::info!("Udp listening on: {}", opt.listen_addr);

    loop {
        let listener = listener.clone();
        let opt = opt.clone();
        let cache = cache.clone();
        let auth = user_key.clone();
        let block = async move {
            let mut buf = vec![0u8; MAX_BUFFER_SIZE];
            let (len, src) = listener.recv_from(&mut buf).await?;
            buf.resize(len, 0);
            tokio::spawn(async move {
                if let Err(e) = udp_incoming_handler(listener, buf, src, opt, cache, auth, timeout).await {
                    log::error!("DNS query via UDP incoming handler error \"{}\"", e);
                }
            });
            Ok::<(), Error>(())
        };
        if let Err(e) = block.await {
            log::error!("UDP listener error \"{}\"", e);
        }
    }
}

async fn udp_incoming_handler(
    listener: Arc<UdpSocket>,
    mut buf: Vec<u8>,
    src: SocketAddr,
    opt: CmdOpt,
    cache: Cache<Vec<Query>, Message>,
    auth: Option<UserKey>,
    timeout: Duration,
) -> Result<()> {
    let message = dns::parse_data_to_dns_message(&buf, false)?;
    let domain = dns::extract_domain_from_dns_message(&message)?;

    if opt.cache_records {
        if let Some(cached_message) = dns_cache_get_message(&cache, &message).await {
            let data = cached_message.to_vec().map_err(|e| e.to_string())?;
            listener.send_to(&data, &src).await?;
            log_dns_message("DNS query via UDP cache hit", &domain, &cached_message);
            return Ok(());
        }
    }

    let proxy_addr = opt.socks5_server;
    let udp_server_addr = opt.dns_remote_server;

    let data = if opt.force_tcp {
        let mut new_buf = (buf.len() as u16).to_be_bytes().to_vec();
        new_buf.append(&mut buf);
        tcp_via_socks5_server(proxy_addr, udp_server_addr, auth, &new_buf, timeout)
            .await
            .map_err(|e| format!("querying \"{domain}\" {e}"))?
    } else {
        client::UdpClientImpl::datagram(proxy_addr, udp_server_addr, auth)
            .await
            .map_err(|e| format!("preparing to query \"{domain}\" {e}"))?
            .transfer_data(&buf, timeout)
            .await
            .map_err(|e| format!("querying \"{domain}\" {e}"))?
    };
    let message = dns::parse_data_to_dns_message(&data, opt.force_tcp)?;
    let msg_buf = message.to_vec().map_err(|e| e.to_string())?;

    listener.send_to(&msg_buf, &src).await?;

    log_dns_message("DNS query via UDP", &domain, &message);
    if opt.cache_records {
        dns_cache_put_message(&cache, &message).await;
    }
    Ok::<(), Error>(())
}

async fn tcp_thread(opt: CmdOpt, user_key: Option<UserKey>, cache: Cache<Vec<Query>, Message>, timeout: Duration) -> Result<()> {
    let listener = TcpListener::bind(&opt.listen_addr).await?;
    log::info!("TCP listening on: {}", opt.listen_addr);

    while let Ok((mut incoming, _)) = listener.accept().await {
        let opt = opt.clone();
        let user_key = user_key.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_incoming(&opt, user_key, cache, &mut incoming, timeout).await {
                log::error!("TCP error \"{}\"", e);
            }
        });
    }
    Ok(())
}

async fn handle_tcp_incoming(
    opt: &CmdOpt,
    auth: Option<UserKey>,
    cache: Cache<Vec<Query>, Message>,
    incoming: &mut TcpStream,
    timeout: Duration,
) -> Result<()> {
    let mut buf = [0u8; MAX_BUFFER_SIZE];
    let n = incoming.read(&mut buf).await?;

    let message = dns::parse_data_to_dns_message(&buf[..n], true)?;
    let domain = dns::extract_domain_from_dns_message(&message)?;

    if opt.cache_records {
        if let Some(cached_message) = dns_cache_get_message(&cache, &message).await {
            let data = cached_message.to_vec().map_err(|e| e.to_string())?;
            let len = u16::try_from(data.len()).map_err(|e| e.to_string())?.to_be_bytes().to_vec();
            let data = [len, data].concat();
            incoming.write_all(&data).await?;
            log_dns_message("DNS query via TCP cache hit", &domain, &cached_message);
            return Ok(());
        }
    }

    let proxy_addr = opt.socks5_server;
    let target_server = opt.dns_remote_server;
    let buf = tcp_via_socks5_server(proxy_addr, target_server, auth, &buf[..n], timeout).await?;

    incoming.write_all(&buf[..n]).await?;

    let message = dns::parse_data_to_dns_message(&buf[..n], true)?;
    log_dns_message("DNS query via TCP", &domain, &message);

    if opt.cache_records {
        dns_cache_put_message(&cache, &message).await;
    }

    Ok(())
}

async fn tcp_via_socks5_server<A, B>(
    proxy_addr: A,
    target_server: B,
    auth: Option<UserKey>,
    buf: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>>
where
    A: ToSocketAddrs,
    B: Into<Address>,
{
    let s5_proxy = TcpStream::connect(proxy_addr).await?;
    let mut stream = BufStream::new(s5_proxy);
    let _addr = client::connect(&mut stream, target_server, auth).await?;

    stream.write_all(buf).await?;
    stream.flush().await?;

    let mut buf = vec![0; MAX_BUFFER_SIZE];
    let n = tokio::time::timeout(timeout, stream.read(&mut buf)).await??;
    Ok(buf[..n].to_vec())
}

fn log_dns_message(prefix: &str, domain: &str, message: &Message) {
    let ipaddr = match dns::extract_ipaddr_from_dns_message(message) {
        Ok(ipaddr) => {
            format!("{:?}", ipaddr)
        }
        Err(e) => e,
    };
    log::trace!("{} {:?} <==> {:?}", prefix, domain, ipaddr);
}

pub(crate) fn create_dns_cache() -> Cache<Vec<Query>, Message> {
    Cache::builder()
        .time_to_live(Duration::from_secs(30 * 60))
        .time_to_idle(Duration::from_secs(5 * 60))
        .build()
}

pub(crate) async fn dns_cache_get_message(cache: &Cache<Vec<Query>, Message>, message: &Message) -> Option<Message> {
    if let Some(mut cached_message) = cache.get(&message.queries().to_vec()).await {
        cached_message.set_id(message.id());
        return Some(cached_message);
    }
    None
}

pub(crate) async fn dns_cache_put_message(cache: &Cache<Vec<Query>, Message>, message: &Message) {
    cache.insert(message.queries().to_vec(), message.clone()).await;
}
