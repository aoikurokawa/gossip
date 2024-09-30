use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient, rpc_response::RpcVoteAccountInfo,
};
use solana_gossip::{
    crds_value::{CrdsValue, CrdsValueLabel},
    gossip_service::make_gossip_node,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::{read_keypair_file, Keypair},
};
use solana_streamer::socket::SocketAddrSpace;
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());
    let active_vote_accounts: HashMap<Pubkey, RpcVoteAccountInfo> = HashMap::from_iter(
        get_vote_accounts_with_retry(&client, 5, None)
            .await
            .unwrap()
            .iter()
            .map(|vote_account_info| {
                (
                    Pubkey::from_str(vote_account_info.vote_pubkey.as_str()).unwrap(),
                    vote_account_info.clone(),
                )
            }),
    );
    let gossip_entrypoint =
        solana_net_utils::parse_host_port("entrypoint.mainnet-beta.solana.com:8001").unwrap();

    let keypair = Arc::new(
        read_keypair_file("~/.config/solana/id.json").expect("Failed reading keypair file"),
    );
    let exit: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let gossip_ip = solana_net_utils::get_public_ip_addr(&gossip_entrypoint).unwrap();
    let gossip_addr = SocketAddr::new(
        gossip_ip,
        solana_net_utils::find_available_port_in_range(IpAddr::V4(Ipv4Addr::UNSPECIFIED), (0, 1))
            .unwrap(),
    );

    let (_gossip_service, _ip_echo, cluster_info) = make_gossip_node(
        Keypair::from_base58_string(keypair.to_base58_string().as_str()),
        Some(&gossip_entrypoint),
        exit,
        Some(&gossip_addr),
        0,
        true,
        SocketAddrSpace::Global,
    );

    sleep(Duration::from_secs(150)).await;

    let crds = cluster_info.gossip.crds.read().unwrap();
    for (_vote_account, vote_account_info) in active_vote_accounts.iter() {
        let validator_identity = Pubkey::from_str(&vote_account_info.node_pubkey).unwrap();
        let _validator_vote_pubkey = Pubkey::from_str(&vote_account_info.vote_pubkey).unwrap();

        let _contact_info_key = CrdsValueLabel::ContactInfo(validator_identity);
        let _legacy_contact_info_key: CrdsValueLabel =
            CrdsValueLabel::LegacyContactInfo(validator_identity);
        let _version_key: CrdsValueLabel = CrdsValueLabel::Version(validator_identity);
        let _legacy_version_key: CrdsValueLabel = CrdsValueLabel::LegacyVersion(validator_identity);
        let restart_last_voted_for_slots_key: CrdsValueLabel =
            CrdsValueLabel::RestartLastVotedForkSlots(validator_identity);
        let restart_heaviest_fork_key: CrdsValueLabel =
            CrdsValueLabel::RestartHeaviestFork(validator_identity);

        if let Some(entry) = crds.get::<&CrdsValue>(&restart_last_voted_for_slots_key) {
            println!("{:?}", entry);
        }

        if let Some(entry) = crds.get::<&CrdsValue>(&restart_heaviest_fork_key) {
            println!("{:?}", entry);
        }
    }
}

pub async fn get_vote_accounts_with_retry(
    client: &RpcClient,
    min_vote_epochs: usize,
    commitment: Option<CommitmentConfig>,
) -> Result<Vec<RpcVoteAccountInfo>, ClientError> {
    for _ in 1..4 {
        let result = client
            .get_vote_accounts_with_commitment(commitment.unwrap_or(CommitmentConfig::finalized()))
            .await;
        if let Ok(response) = result {
            return Ok(response
                .current
                .into_iter()
                .chain(response.delinquent.into_iter())
                .filter(|vote_account| vote_account.epoch_credits.len() >= min_vote_epochs)
                .collect::<Vec<_>>());
        }
    }
    let result = client
        .get_vote_accounts_with_commitment(commitment.unwrap_or(CommitmentConfig::finalized()))
        .await;
    match result {
        Ok(response) => Ok(response
            .current
            .into_iter()
            .chain(response.delinquent.into_iter())
            .filter(|vote_account| vote_account.epoch_credits.len() >= min_vote_epochs)
            .collect::<Vec<_>>()),
        Err(e) => Err(e),
    }
}
