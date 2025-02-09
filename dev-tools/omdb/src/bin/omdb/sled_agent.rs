// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! omdb commands that query or update specific Sleds

use crate::Omdb;
use anyhow::bail;
use anyhow::Context;
use clap::Args;
use clap::Subcommand;

/// Arguments to the "omdb sled-agent" subcommand
#[derive(Debug, Args)]
pub struct SledAgentArgs {
    /// URL of the Sled internal API
    #[clap(long, env("OMDB_SLED_AGENT_URL"))]
    sled_agent_url: Option<String>,

    #[command(subcommand)]
    command: SledAgentCommands,
}

/// Subcommands for the "omdb sled-agent" subcommand
#[derive(Debug, Subcommand)]
enum SledAgentCommands {
    /// print information about zones
    #[clap(subcommand)]
    Zones(ZoneCommands),

    /// print information about zpools
    #[clap(subcommand)]
    Zpools(ZpoolCommands),
}

#[derive(Debug, Subcommand)]
enum ZoneCommands {
    /// Print list of all running control plane zones
    List,
}

#[derive(Debug, Subcommand)]
enum ZpoolCommands {
    /// Print list of all zpools managed by the sled agent
    List,
}

impl SledAgentArgs {
    /// Run a `omdb sled-agent` subcommand.
    pub(crate) async fn run_cmd(
        &self,
        _omdb: &Omdb,
        log: &slog::Logger,
    ) -> Result<(), anyhow::Error> {
        // This is a little goofy. The sled URL is required, but can come
        // from the environment, in which case it won't be on the command line.
        let Some(sled_agent_url) = &self.sled_agent_url else {
            bail!(
                "sled URL must be specified with --sled-agent-url or \
                OMDB_SLED_AGENT_URL"
            );
        };
        let client =
            sled_agent_client::Client::new(sled_agent_url, log.clone());

        match &self.command {
            SledAgentCommands::Zones(ZoneCommands::List) => {
                cmd_zones_list(&client).await
            }
            SledAgentCommands::Zpools(ZpoolCommands::List) => {
                cmd_zpools_list(&client).await
            }
        }
    }
}

/// Runs `omdb sled-agent zones list`
async fn cmd_zones_list(
    client: &sled_agent_client::Client,
) -> Result<(), anyhow::Error> {
    let response = client.zones_list().await.context("listing zones")?;
    let zones = response.into_inner();
    let zones: Vec<_> = zones.into_iter().collect();

    println!("zones:");
    if zones.is_empty() {
        println!("    <none>");
    }
    for zone in &zones {
        println!("    {:?}", zone);
    }

    Ok(())
}

/// Runs `omdb sled-agent zpools list`
async fn cmd_zpools_list(
    client: &sled_agent_client::Client,
) -> Result<(), anyhow::Error> {
    let response = client.zpools_get().await.context("listing zpools")?;
    let zpools = response.into_inner();

    println!("zpools:");
    if zpools.is_empty() {
        println!("    <none>");
    }
    for zpool in &zpools {
        println!("    {:?}", zpool);
    }

    Ok(())
}
