// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Executable program to run the sled agent

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use omicron_common::cmd::fatal;
use omicron_common::cmd::CmdError;
use omicron_sled_agent::bootstrap::server as bootstrap_server;
use omicron_sled_agent::bootstrap::RssAccessError;
use omicron_sled_agent::rack_setup::config::SetupServiceConfig as RssConfig;
use omicron_sled_agent::{config::Config as SledConfig, server as sled_server};

#[derive(Subcommand, Debug)]
enum OpenapiFlavor {
    /// Generates bootstrap agent openapi spec
    Bootstrap,
    /// Generates sled agent openapi spec
    Sled,
}

#[derive(Debug, Parser)]
#[clap(
    name = "sled_agent",
    about = "See README.adoc for more information",
    version
)]
enum Args {
    /// Generates the OpenAPI specification.
    #[command(subcommand)]
    Openapi(OpenapiFlavor),

    /// Runs the Sled Agent server.
    Run {
        #[clap(name = "CONFIG_FILE_PATH", action)]
        config_path: Utf8PathBuf,
    },
}

#[tokio::main]
async fn main() {
    if let Err(message) = do_run().await {
        fatal(message);
    }
}

async fn do_run() -> Result<(), CmdError> {
    let args = Args::parse();

    match args {
        Args::Openapi(flavor) => match flavor {
            OpenapiFlavor::Sled => {
                sled_server::run_openapi().map_err(CmdError::Failure)
            }
            OpenapiFlavor::Bootstrap => {
                bootstrap_server::run_openapi().map_err(CmdError::Failure)
            }
        },
        Args::Run { config_path } => {
            let config = SledConfig::from_file(&config_path)
                .map_err(|e| CmdError::Failure(e.to_string()))?;

            // - Sled agent starts with the normal config file - typically
            // called "config.toml".
            // - Thing-flinger likes allowing "sled-specific" configs to arrive
            // by overlaying files in the package...
            // - ... so we need a way to *possibly* supply this extra config,
            // without otherwise changing the package.
            //
            // This means we must possibly ingest a config file, without
            // *explicitly* being told about it.
            //
            // Hence, this approach: look around in the same directory as the
            // expected config file.
            let rss_config_path = {
                let mut rss_config_path = config_path.clone();
                rss_config_path.pop();
                rss_config_path.push("config-rss.toml");
                rss_config_path
            };
            let rss_config = if rss_config_path.exists() {
                Some(
                    RssConfig::from_file(rss_config_path)
                        .map_err(|e| CmdError::Failure(e.to_string()))?,
                )
            } else {
                None
            };

            let server = bootstrap_server::Server::start(config)
                .await
                .map_err(|err| CmdError::Failure(format!("{err:#}")))?;

            // If requested, automatically supply the RSS configuration.
            //
            // This should remain equivalent to the HTTP request which can
            // be invoked by Wicket.
            if let Some(rss_config) = rss_config {
                match server.start_rack_initialize(rss_config) {
                    // If the rack has already been initialized, we shouldn't
                    // abandon the server.
                    Ok(_) | Err(RssAccessError::AlreadyInitialized) => {}
                    Err(e) => {
                        return Err(CmdError::Failure(e.to_string()));
                    }
                }
            }

            server.wait_for_finish().await.map_err(CmdError::Failure)?;

            Ok(())
        }
    }
}
