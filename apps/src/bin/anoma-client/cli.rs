//! Anoma client CLI.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;

use anoma::types::intent::{Exchange, FungibleTokenIntent};
use anoma::types::key::ed25519::Signed;
use anoma_apps::cli::{args, cmds, Context};
use anoma_apps::client::{rpc, tx};
use anoma_apps::proto::services::rpc_service_client::RpcServiceClient;
use anoma_apps::proto::{services, RpcMessage};
use anoma_apps::{cli, wallet};
use borsh::BorshSerialize;
use color_eyre::eyre::Result;

pub async fn main() -> Result<()> {
    let (cmd, ctx) = cli::anoma_client_cli();
    match cmd {
        cmds::AnomaClient::TxCustom(cmds::TxCustom(args)) => {
            tx::submit_custom(&ctx, args).await;
        }
        cmds::AnomaClient::TxTransfer(cmds::TxTransfer(args)) => {
            tx::submit_transfer(&ctx, args).await;
        }
        cmds::AnomaClient::TxUpdateVp(cmds::TxUpdateVp(args)) => {
            tx::submit_update_vp(&ctx, args).await;
        }
        cmds::AnomaClient::QueryBalance(cmds::QueryBalance(args)) => {
            rpc::query_balance(&ctx, args).await;
        }
        cmds::AnomaClient::Intent(cmds::Intent(args)) => {
            gossip_intent(&ctx, args).await;
        }
        cmds::AnomaClient::CraftIntent(cmds::CraftIntent(args)) => {
            craft_intent(&ctx, args);
        }
        cmds::AnomaClient::SubscribeTopic(cmds::SubscribeTopic(args)) => {
            subscribe_topic(&ctx, args).await;
        }
    }
    Ok(())
}

async fn gossip_intent(
    _ctx: &Context,
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
    _ctx: &Context,
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
    _ctx: &Context,
    args::CraftIntent {
        key,
        exchanges,
        file_path,
    }: args::CraftIntent,
) {
    let signed_exchanges: HashSet<Signed<Exchange>> = exchanges
        .iter()
        .map(|exchange| {
            let source_keypair =
                wallet::defaults::key_of(exchange.addr.encode());
            Signed::new(&source_keypair, exchange.clone())
        })
        .collect();

    let signing_key = wallet::defaults::key_of(key.encode());
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
