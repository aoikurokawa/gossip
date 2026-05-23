use anyhow::{anyhow, Context, Result};
use clap::Parser;
use log::*;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_gossip::contact_info::ContactInfo;
use solana_gossip::gossip_service::make_node;
use solana_net_utils::socket_addr_space::SocketAddrSpace;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

// IP echo server protocol constants
// https://github.com/anza-xyz/agave/blob/master/net-utils/src/ip_echo_server.rs
const IP_ECHO_RESPONSE_LEN: usize = 27;
const IP_ADDR_OFFSET_V4: usize = 8;
const SHRED_VERSION_OFFSET: usize = 4 + IP_ADDR_OFFSET_V4;
const IP_ECHO_REQUEST: &[u8] = &[0x00; 21];

#[derive(Parser, Debug)]
#[command(about = "Subscribe to Solana gossip and print validator info")]
struct Args {
    /// Gossip entrypoint address (host:port), e.g. entrypoint.mainnet-beta.solana.com:8001
    #[arg(short, long)]
    entrypoint: Vec<String>,

    /// RPC URL for vote account lookup, e.g. https://api.mainnet-beta.solana.com
    #[arg(short, long)]
    rpc_url: Option<String>,

    /// Seconds to wait for gossip discovery
    #[arg(short, long, default_value_t = 60)]
    wait: u64,

    /// Filter to a specific validator identity pubkey
    #[arg(short, long)]
    identity: Option<String>,

    /// Only show nodes that have a corresponding vote account (requires --rpc-url)
    #[arg(long)]
    validators_only: bool,
}

struct EchoResponse {
    ip: IpAddr,
    shred_version: Option<u16>,
}

impl TryFrom<&[u8]> for EchoResponse {
    type Error = anyhow::Error;

    fn try_from(data: &[u8]) -> Result<Self> {
        if data.len() < IP_ECHO_RESPONSE_LEN {
            return Err(anyhow!(
                "expected {} bytes, got {}",
                IP_ECHO_RESPONSE_LEN,
                data.len()
            ));
        }
        let octets = &data[IP_ADDR_OFFSET_V4..IP_ADDR_OFFSET_V4 + 4];
        let ip = IpAddr::V4(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]));
        let shred_version = if data[SHRED_VERSION_OFFSET] == 0 {
            None
        } else {
            let b = &data[SHRED_VERSION_OFFSET..SHRED_VERSION_OFFSET + 3];
            Some(u16::from_le_bytes([b[1], b[2]]))
        };
        Ok(EchoResponse { ip, shred_version })
    }
}

async fn fetch_ip_and_shred_version(entrypoint: &str) -> Result<EchoResponse> {
    let mut stream = TcpStream::connect(entrypoint)
        .await
        .with_context(|| format!("failed to connect to {entrypoint}"))?;
    stream.write_all(IP_ECHO_REQUEST).await?;
    stream.flush().await?;
    let mut buf = vec![0u8; IP_ECHO_RESPONSE_LEN];
    let n = stream.read(&mut buf).await?;
    if n != IP_ECHO_RESPONSE_LEN {
        return Err(anyhow!("expected {IP_ECHO_RESPONSE_LEN} bytes, got {n}"));
    }
    EchoResponse::try_from(&buf[..n])
}

// Build identity → vote_pubkey map from getVoteAccounts RPC call.
// Both current and delinquent validators are included.
async fn fetch_vote_account_map(rpc_url: &str) -> Result<HashMap<Pubkey, Pubkey>> {
    let client = RpcClient::new(rpc_url.to_string());
    let response = client
        .get_vote_accounts()
        .await
        .context("getVoteAccounts RPC call failed")?;

    let mut map = HashMap::new();
    for va in response.current.iter().chain(response.delinquent.iter()) {
        if let (Ok(identity), Ok(vote)) = (
            Pubkey::from_str(&va.node_pubkey),
            Pubkey::from_str(&va.vote_pubkey),
        ) {
            map.insert(identity, vote);
        }
    }
    info!(
        "fetched {} vote accounts ({} current, {} delinquent)",
        map.len(),
        response.current.len(),
        response.delinquent.len()
    );
    Ok(map)
}

fn print_node(
    ci: &ContactInfo,
    cluster_info: &solana_gossip::cluster_info::ClusterInfo,
    vote_map: &HashMap<Pubkey, Pubkey>,
) {
    let pubkey = ci.pubkey();
    let gossip = ci
        .gossip()
        .map(|a| a.to_string())
        .unwrap_or_else(|| "none".to_string());
    let wallclock = ci.wallclock();
    let shred_version = ci.shred_version();

    // get_node_version uses the 4.0 Version struct which correctly decodes
    // the PackedMinor format including prerelease (rc/beta/alpha) suffixes
    let version = cluster_info
        .get_node_version(pubkey)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let vote_account = vote_map
        .get(pubkey)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_string());

    println!(
        "identity={pubkey}  vote={vote_account}  gossip={gossip}  version={version}  shred={shred_version}  wallclock={wallclock}"
    );
}

async fn run_for_entrypoints(
    entrypoints: &[SocketAddr],
    rpc_url: Option<&str>,
    wait_secs: u64,
    filter: Option<Pubkey>,
    validators_only: bool,
) -> Result<()> {
    // Fetch vote accounts from RPC (if URL provided) concurrently with IP echo
    let vote_map_future = async {
        match rpc_url {
            Some(url) => fetch_vote_account_map(url).await,
            None => Ok(HashMap::new()),
        }
    };

    // Use the first entrypoint to discover our public IP and the cluster shred version
    let first = entrypoints
        .first()
        .ok_or_else(|| anyhow!("no entrypoints provided"))?;
    info!("fetching public IP and shred version from {first}");

    let first_str = first.to_string();
    let (echo_result, vote_map) =
        tokio::join!(fetch_ip_and_shred_version(&first_str), vote_map_future);

    let echo = echo_result?;
    let vote_map = vote_map?;
    let gossip_ip = echo.ip;
    let shred_version = echo.shred_version.unwrap_or(0);
    info!("public IP={gossip_ip}  shred_version={shred_version}");

    let gossip_port =
        solana_net_utils::find_available_port_in_range(IpAddr::V4(Ipv4Addr::UNSPECIFIED), (0, 1))
            .expect("no available gossip port");
    let gossip_addr = SocketAddr::new(gossip_ip, gossip_port);

    let exit = Arc::new(AtomicBool::new(false));
    let keypair = Keypair::new();

    let (_gossip_service, _ip_echo, cluster_info) = make_node(
        keypair,
        entrypoints,
        exit.clone(),
        Some(&gossip_addr),
        shred_version,
        true,
        SocketAddrSpace::Global,
    );

    info!("gossip node started on {gossip_addr}, waiting {wait_secs}s for discovery...");
    sleep(Duration::from_secs(wait_secs)).await;

    // all_peers() returns (ContactInfo, local_timestamp) pairs filtered by shred_version
    let all_peers = cluster_info.all_peers();
    let total = all_peers.len();

    let peers: Vec<_> = all_peers
        .into_iter()
        .filter(|(ci, _)| {
            if let Some(pk) = filter {
                ci.pubkey() == &pk
            } else if validators_only {
                vote_map.contains_key(ci.pubkey())
            } else {
                true
            }
        })
        .collect();

    if validators_only {
        println!(
            "\n--- validators (with vote account): {} / {} total gossip nodes ---",
            peers.len(),
            total
        );
    } else {
        println!("\n--- gossip nodes discovered: {} ---", peers.len());
    }

    for (ci, _ts) in &peers {
        print_node(ci, &cluster_info, &vote_map);
    }

    exit.store(true, Ordering::Relaxed);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    if args.entrypoint.is_empty() {
        return Err(anyhow!("provide at least one --entrypoint"));
    }

    let filter = args
        .identity
        .as_deref()
        .map(Pubkey::from_str)
        .transpose()
        .context("invalid identity pubkey")?;

    let entrypoints: Vec<SocketAddr> = args
        .entrypoint
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            if s.is_empty() {
                return None;
            }
            Some(
                solana_net_utils::parse_host_port(s).unwrap_or_else(|e| {
                    panic!("failed to parse entrypoint #{} '{}': {}", i + 1, s, e)
                }),
            )
        })
        .collect();

    if args.validators_only && args.rpc_url.is_none() {
        return Err(anyhow!("--validators-only requires --rpc-url"));
    }

    if let Err(e) = run_for_entrypoints(
        &entrypoints,
        args.rpc_url.as_deref(),
        args.wait,
        filter,
        args.validators_only,
    )
    .await
    {
        error!("{e:#}");
        std::process::exit(1);
    }

    Ok(())
}
