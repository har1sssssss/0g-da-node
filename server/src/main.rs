#[macro_use]
extern crate tracing;

mod config;
mod context;
mod runtime;

use std::{error::Error, net::SocketAddr, str::FromStr, sync::Arc};

use anyhow::{anyhow, Result};

use chain_state::{
    da_handler::start_da_monitor, signers_handler::start_epoch_registration, ChainState,
};
use da_miner::DasMineService;
use grpc::run_server;

use task_executor::TaskExecutor;
use tracing::Level;

use crate::config::Config;
use crate::context::Context;
use crate::runtime::make_environment;

async fn start_grpc_server(chain_state: Arc<ChainState>, ctx: &Context) -> Result<()> {
    let db = ctx.db.clone();
    let signer_private_key = ctx.config.signer_private_key;
    let grpc_listen_address = ctx.config.grpc_listen_address.clone();
    let encoder_params_dir = ctx.config.encoder_params_dir.clone();
    let max_ongoing_sign_request = ctx.config.max_ongoing_sign_request;
    info!("starting grpc server at {:?}", grpc_listen_address);
    tokio::spawn(async move {
        run_server(
            db,
            chain_state,
            signer_private_key,
            SocketAddr::from_str(&grpc_listen_address).unwrap(),
            encoder_params_dir,
            max_ongoing_sign_request,
        )
        .await
        .map_err(|e| anyhow!(e.to_string()))
        .unwrap();
    });
    Ok(())
}

async fn setup_chain_state(ctx: &Context) -> Result<Arc<ChainState>> {
    let chain_state = Arc::new(
        ChainState::new(
            &ctx.config.eth_rpc_url,
            ctx.config.da_entrance_address,
            ctx.transactor.clone(),
            ctx.db.clone(),
        )
        .await?,
    );
    chain_state
        .check_signer_registration(
            ctx.config.signer_private_key,
            ctx.config.socket_address.clone(),
        )
        .await?;
    start_epoch_registration(chain_state.clone(), ctx.config.signer_private_key);
    start_da_monitor(chain_state.clone(), ctx.config.start_block_number).await?;
    Ok(chain_state)
}

async fn start_server(ctx: &Context) -> Result<()> {
    let chain_state = setup_chain_state(ctx).await?;
    start_grpc_server(chain_state.clone(), ctx).await?;
    Ok(())
}

async fn start_das_service(env: runtime::Environment, executor: TaskExecutor, ctx: &Context) {
    if !ctx.config.enable_das {
        return;
    }

    DasMineService::spawn(
        executor,
        ctx.provider.clone(),
        ctx.config.da_entrance_address,
        ctx.db.clone(),
    )
    .await
    .unwrap();
    info!("DA sampling mine service started");
    env.wait_shutdown_signal().await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // enable backtraces
    std::env::set_var("RUST_BACKTRACE", "1");

    // CLI, config
    let config = Config::from_cli_file().unwrap();
    let ctx = Context::new(config).await.unwrap();

    // rayon
    if let Some(num_threads) = ctx.config.max_verify_threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build_global()
            .unwrap();
    }

    // tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::from_str(&ctx.config.log_level).unwrap())
        .init();

    let (enviroment, executor) = make_environment().unwrap();
    let das_service = start_das_service(enviroment, executor, &ctx);
    let rpc_service = start_server(&ctx);

    tokio::select! {
        res = rpc_service => {
            if let Err(e) = res {
                error!(error = ?e, "Signer service exit with error");
                std::process::exit(1);
            } else {
                info!("Signer service exit");
            }
        },
        _ = das_service, if ctx.config.enable_das => {
            info!("Das service signal received, stopping..");
        }
    }
    Ok(())
}
