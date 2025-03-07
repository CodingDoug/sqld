use anyhow::Result;
use aws_sdk_s3::Client;
use chrono::NaiveDateTime;
use clap::{Parser, Subcommand};

mod replicator_extras;
use crate::replicator_extras::detect_db;
use replicator_extras::Replicator;

#[derive(Debug, Parser)]
#[command(name = "bottomless-cli")]
#[command(about = "Bottomless CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    #[clap(long, short)]
    endpoint: Option<String>,
    #[clap(long, short)]
    bucket: Option<String>,
    #[clap(long, short)]
    database: Option<String>,
    #[clap(long, short)]
    namespace: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[clap(about = "List available generations")]
    Ls {
        #[clap(long, short, long_help = "List details about single generation")]
        generation: Option<uuid::Uuid>,
        #[clap(
            long,
            short,
            conflicts_with = "generation",
            long_help = "List only <limit> newest generations"
        )]
        limit: Option<u64>,
        #[clap(
            long,
            conflicts_with = "generation",
            long_help = "List only generations older than given date"
        )]
        older_than: Option<chrono::NaiveDate>,
        #[clap(
            long,
            conflicts_with = "generation",
            long_help = "List only generations newer than given date"
        )]
        newer_than: Option<chrono::NaiveDate>,
        #[clap(
            long,
            short,
            long_help = "Print detailed information on each generation"
        )]
        verbose: bool,
    },
    #[clap(about = "Restore the database")]
    Restore {
        #[clap(
            long,
            short,
            long_help = "Generation to restore from.\nSkip this parameter to restore from the newest generation."
        )]
        generation: Option<uuid::Uuid>,
        #[clap(
            long,
            short,
            conflicts_with = "generation",
            long_help = "UTC timestamp which is an upper bound for the transactions to be restored."
        )]
        utc_time: Option<NaiveDateTime>,
    },
    #[clap(about = "Remove given generation from remote storage")]
    Rm {
        #[clap(long, short)]
        generation: Option<uuid::Uuid>,
        #[clap(
            long,
            conflicts_with = "generation",
            long_help = "Remove generations older than given date"
        )]
        older_than: Option<chrono::NaiveDate>,
        #[clap(long, short)]
        verbose: bool,
    },
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();
    let mut options = Cli::parse();

    if let Some(ep) = options.endpoint.as_deref() {
        std::env::set_var("LIBSQL_BOTTOMLESS_ENDPOINT", ep)
    } else {
        options.endpoint = std::env::var("LIBSQL_BOTTOMLESS_ENDPOINT").ok();
    }

    if let Some(bucket) = options.bucket.as_deref() {
        std::env::set_var("LIBSQL_BOTTOMLESS_BUCKET", bucket)
    } else {
        options.bucket = std::env::var("LIBSQL_BOTTOMLESS_BUCKET").ok();
    }
    let namespace = options.namespace.as_deref().unwrap_or("ns-default");
    std::env::set_var("LIBSQL_BOTTOMLESS_DATABASE_ID", namespace);
    let database = match options.database.clone() {
        Some(db) => db,
        None => {
            let client = Client::from_conf({
                let mut loader = aws_config::from_env();
                if let Some(endpoint) = options.endpoint.clone() {
                    loader = loader.endpoint_url(endpoint);
                }
                aws_sdk_s3::config::Builder::from(&loader.load().await)
                    .force_path_style(true)
                    .build()
            });
            let bucket = options.bucket.as_deref().unwrap_or("bottomless");
            match detect_db(&client, bucket, namespace).await {
                Some(db) => db,
                None => {
                    println!("Could not autodetect the database. Please pass it explicitly with -d option");
                    return Ok(());
                }
            }
        }
    };
    let database = database + "/dbs/" + namespace.strip_prefix("ns-").unwrap() + "/data";
    tracing::info!("Database: '{}' (namespace: {})", database, namespace);

    let mut client = Replicator::new(database.clone()).await?;

    match options.command {
        Commands::Ls {
            generation,
            limit,
            older_than,
            newer_than,
            verbose,
        } => match generation {
            Some(gen) => client.list_generation(gen).await?,
            None => {
                client
                    .list_generations(limit, older_than, newer_than, verbose)
                    .await?
            }
        },
        Commands::Restore {
            generation,
            utc_time,
        } => {
            tokio::fs::create_dir_all(&database).await?;
            client.restore(generation, utc_time).await?;
        }
        Commands::Rm {
            generation,
            older_than,
            verbose,
        } => match (generation, older_than) {
            (None, Some(older_than)) => client.remove_many(older_than, verbose).await?,
            (Some(generation), None) => client.remove(generation, verbose).await?,
            (Some(_), Some(_)) => unreachable!(),
            (None, None) => println!(
                "rm command cannot be run without parameters; see -h or --help for details"
            ),
        },
    };
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e}");
        std::process::exit(1)
    }
}
