//! Anoma client CLI.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;

use anoma::types::intent::{Exchange, FungibleTokenIntent};
use anoma::types::key::ed25519::Signed;
use anoma_apps::cli::{args, cmds};
use anoma_apps::client::{rpc, tx};
use anoma_apps::proto::services::rpc_service_client::RpcServiceClient;
use anoma_apps::proto::{services, RpcMessage};
use anoma_apps::{cli, wallet};
use borsh::BorshSerialize;
use color_eyre::eyre::Result;

pub async fn main() -> Result<()> {
    let (cmd, _global_args) = cli::anoma_client_cli();
    match cmd {
        // Ledger cmds
        cmds::AnomaClient::TxCustom(cmds::TxCustom(args)) => {
            tx::submit_custom(args).await;
        }
        cmds::AnomaClient::TxTransfer(cmds::TxTransfer(args)) => {
            tx::submit_transfer(args).await;
        }
        cmds::AnomaClient::TxUpdateVp(cmds::TxUpdateVp(args)) => {
            tx::submit_update_vp(args).await;
        }
        cmds::AnomaClient::Bond(cmds::Bond(args)) => {
            tx::submit_bond(args).await;
        }
        cmds::AnomaClient::Unbond(cmds::Unbond(args)) => {
            tx::submit_unbond(args).await;
        }
        cmds::AnomaClient::Withdraw(cmds::Withdraw(args)) => {
            tx::submit_withdraw(args).await;
        }
        cmds::AnomaClient::QueryEpoch(cmds::QueryEpoch(args)) => {
            rpc::query_epoch(args).await;
        }
        cmds::AnomaClient::QueryBalance(cmds::QueryBalance(args)) => {
            rpc::query_balance(args).await;
        }
        cmds::AnomaClient::QueryBonds(cmds::QueryBonds(args)) => {
            rpc::query_bonds(args).await;
        }
        cmds::AnomaClient::QueryVotingPower(cmds::QueryVotingPower(args)) => {
            rpc::query_voting_power(args).await;
        }
        cmds::AnomaClient::QuerySlashes(cmds::QuerySlashes(args)) => {
            rpc::query_slashes(args).await;
        }
        // Gossip cmds
        cmds::AnomaClient::Intent(cmds::Intent(args)) => {
            gossip_intent(args).await;
        }
        cmds::AnomaClient::CraftIntent(cmds::CraftIntent(args)) => {
            craft_intent(args);
        }
        cmds::AnomaClient::SubscribeTopic(cmds::SubscribeTopic(args)) => {
            subscribe_topic(args).await;
        }
    }
    Ok(())
}

async fn gossip_intent(
    args::Intent {
        node_addr,
        data_path,
        topic,
    }: args::Intent,
) {
    let mut client = RpcServiceClient::connect(node_addr).await.unwrap();
    let data = std::fs::read(data_path).expect("data file IO error");
    let intent = anoma::proto::Intent::new(data);
    let message: services::RpcMessage =
        RpcMessage::new_intent(intent, topic).into();
    let response = client
        .send_message(message)
        .await
        .expect("failed to send message and/or receive rpc response");
    println!("{:#?}", response);
}

async fn subscribe_topic(
    args::SubscribeTopic { node_addr, topic }: args::SubscribeTopic,
) {
    let mut client = RpcServiceClient::connect(node_addr).await.unwrap();
    let message: services::RpcMessage = RpcMessage::new_topic(topic).into();
    let response = client
        .send_message(message)
        .await
        .expect("failed to send message and/or receive rpc response");
    println!("{:#?}", response);
}

fn craft_intent(
    args::CraftIntent {
        key,
        exchanges,
        file_path,
    }: args::CraftIntent,
) {
    let signed_exchanges: HashSet<Signed<Exchange>> = exchanges
        .iter()
        .map(|exchange| {
            let source_keypair = wallet::key_of(exchange.addr.encode());
            Signed::new(&source_keypair, exchange.clone())
        })
        .collect();

    let signing_key = wallet::key_of(key.encode());
    let signed_ft: Signed<FungibleTokenIntent> = Signed::new(
        &signing_key,
        FungibleTokenIntent {
            exchange: signed_exchanges,
        },
    );
    let data_bytes = signed_ft.try_to_vec().unwrap();

    let mut file = File::create(file_path).unwrap();
    file.write_all(&data_bytes).unwrap();
}
