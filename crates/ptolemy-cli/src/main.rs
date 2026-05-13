// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "ptolemy", about = "Versioned geodatabase & collaboration platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the API server
    Serve {
        /// Listen address
        #[arg(long, default_value = "0.0.0.0:3000")]
        bind: String,

        /// PostgreSQL connection URL
        #[arg(long, env = "DATABASE_URL")]
        database_url: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve { bind, database_url } => {
            let pool = sqlx::PgPool::connect(&database_url).await?;
            let state = Arc::new(ptolemy_storage::postgres::PgStore::new(pool));
            let app = ptolemy_api::app(state);

            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!("Ptolemy listening on {bind}");
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}
