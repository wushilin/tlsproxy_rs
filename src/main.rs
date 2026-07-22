#![allow(clippy::needless_return)]
#![allow(clippy::option_map_unit_fn)]
#![allow(clippy::too_many_arguments)]

pub mod accounting;
pub mod acme;
pub mod acme_challenge;
pub mod acme_types;
pub mod active_tracker;
pub mod auth;
pub mod bindaddr;
pub mod bootstrap;
pub mod ca;
pub mod certificate;
pub mod config;
pub mod controller;
pub mod control_api;
pub mod extensible;
pub mod forward;
pub mod hello_cache;
pub mod hostutil;
pub mod http_header;
pub mod idle_tracker;
pub mod listener_stats;
pub mod listener;
pub mod managed_tls;
pub mod request_id;
pub mod relay;
pub mod resolver;
pub mod upstream_tls;
pub mod runtime_config;
pub mod runtime;
pub mod store;
pub mod tls_header;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use log::info;
use std::net::IpAddr;
use std::path::PathBuf;

const DEFAULT_RUNTIME_DIR: &str = "tlsproxy-data";

#[derive(Debug, Parser)]
#[command(
    name = "tlsproxy",
    version,
    about = "TLS passthrough and termination proxy with an embedded admin UI",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run from the RocksDB runtime directory; starts one-time setup if empty.
    Run(RunArgs),
    /// Explicitly import a legacy YAML configuration into a new runtime DB.
    Migrate(MigrateArgs),
    /// Create a consistent RocksDB checkpoint while the service is stopped or running.
    Backup(BackupArgs),
    /// Restore a checkpoint into a new, empty runtime directory.
    Restore(RestoreArgs),
    /// Reset or create a local administrator while the service is stopped.
    RecoverAdmin(RecoverAdminArgs),
    /// Run bounded session, certificate-generation, audit, and revision retention.
    Cleanup(CleanupArgs),
}

#[derive(Debug, Args)]
struct BackupArgs { #[arg(long, default_value = DEFAULT_RUNTIME_DIR)] runtime_dir: PathBuf, #[arg(long)] output: PathBuf }
#[derive(Debug, Args)]
struct RestoreArgs { #[arg(long)] checkpoint: PathBuf, #[arg(long, default_value = DEFAULT_RUNTIME_DIR)] runtime_dir: PathBuf }
#[derive(Debug, Args)]
struct RecoverAdminArgs { #[arg(long, default_value = DEFAULT_RUNTIME_DIR)] runtime_dir: PathBuf, #[arg(long, default_value = "admin")] username: String, #[arg(long)] password_file: PathBuf }
#[derive(Debug, Args)]
struct CleanupArgs { #[arg(long, default_value = DEFAULT_RUNTIME_DIR)] runtime_dir: PathBuf, #[arg(long, default_value_t = 3)] generations: usize, #[arg(long, default_value_t = 90)] audit_days: i64 }

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long, default_value = DEFAULT_RUNTIME_DIR)]
    runtime_dir: PathBuf,
    /// Temporary first-run HTTPS setup port. Ignored after initialization.
    #[arg(long = "setup-port", visible_alias = "port")]
    setup_port: Option<u16>,
    #[arg(long, default_value = "127.0.0.1")]
    setup_bind: IpAddr,
    #[arg(long, conflicts_with = "setup_token_file")]
    setup_token: Option<String>,
    #[arg(long, conflicts_with = "setup_token")]
    setup_token_file: Option<PathBuf>,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            runtime_dir: DEFAULT_RUNTIME_DIR.into(),
            setup_port: None,
            setup_bind: "127.0.0.1".parse().unwrap(),
            setup_token: None,
            setup_token_file: None,
        }
    }
}

#[derive(Debug, Args)]
struct MigrateArgs {
    #[arg(short = 'c', long = "config", default_value = "config.yaml")]
    config: PathBuf,
    #[arg(long, default_value = DEFAULT_RUNTIME_DIR)]
    runtime_dir: PathBuf,
    #[arg(long, default_value = "admin")]
    admin_username: String,
    /// File containing the initial administrator password.
    #[arg(long)]
    admin_password_file: PathBuf,
    #[arg(long)]
    control_hostname: String,
    #[arg(long = "self-ip", required = true)]
    self_ips: Vec<IpAddr>,
    #[arg(long, default_value = "letsencrypt-production")]
    provider_id: String,
}

/// Logs go to stdout so the supervising process manager can capture them.
/// `RUST_LOG` overrides the default `info` filter.
fn init_logging() {
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run(RunArgs::default())) {
        Command::Run(args) => run(args).await,
        Command::Migrate(args) => migrate(args).await,
        Command::Backup(args) => backup(args),
        Command::Restore(args) => restore(args),
        Command::RecoverAdmin(args) => recover_admin(args),
        Command::Cleanup(args) => cleanup(args),
    }
}

fn backup(args: BackupArgs) -> Result<()> {
    let store = store::Store::open(&args.runtime_dir)?;
    store.checkpoint(&args.output)?;
    info!("RocksDB checkpoint created at {}", args.output.display());
    Ok(())
}

fn restore(args: RestoreArgs) -> Result<()> {
    anyhow::ensure!(args.checkpoint.is_dir(), "checkpoint must be a directory");
    anyhow::ensure!(!args.runtime_dir.exists() || std::fs::read_dir(&args.runtime_dir)?.next().is_none(), "restore runtime directory must not exist or must be empty");
    std::fs::create_dir_all(&args.runtime_dir)?;
    let target = args.runtime_dir.join("tlsproxy.rocksdb");
    copy_directory(&args.checkpoint, &target)?;
    let store = store::Store::open(&args.runtime_dir)?;
    anyhow::ensure!(store.is_initialized()?, "restored checkpoint is not initialized");
    info!("checkpoint restored into {}", args.runtime_dir.display());
    Ok(())
}

fn copy_directory(source: &std::path::Path, target: &std::path::Path) -> Result<()> {
    std::fs::create_dir(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() { copy_directory(&entry.path(), &destination)?; }
        else { std::fs::copy(entry.path(), destination)?; }
    }
    Ok(())
}

fn recover_admin(args: RecoverAdminArgs) -> Result<()> {
    let password = std::fs::read_to_string(&args.password_file)
        .with_context(|| format!("failed to read `{}`", args.password_file.display()))?;
    let store = store::Store::open(&args.runtime_dir)?;
    anyhow::ensure!(store.is_initialized()?, "runtime is not initialized");
    store.save_user(&auth::UserRecord { username: args.username.clone(), password_hash: auth::hash_password(password.trim())?, administrator: true, disabled: false, created_at: Some(time::OffsetDateTime::now_utc()) })?;
    store.delete_user_sessions(&args.username)?;
    store.append_audit("administrator_recovered", serde_json::json!({"username": args.username}))?;
    info!("administrator recovered and existing sessions revoked");
    Ok(())
}

fn cleanup(args: CleanupArgs) -> Result<()> {
    anyhow::ensure!(args.generations > 0, "at least one generation must be retained");
    let store = store::Store::open(&args.runtime_dir)?;
    let summary = store.cleanup_retention(time::OffsetDateTime::now_utc(), args.generations, args.audit_days)?;
    info!("retention cleanup completed: {summary:?}");
    Ok(())
}

async fn run(args: RunArgs) -> Result<()> {
    // Select a rustls crypto provider before any TLS component is constructed.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();
    let store = store::Store::open(&args.runtime_dir)?;
    if !store.is_initialized()? {
        let token = match (args.setup_token, args.setup_token_file) {
            (Some(token), None) => Some(token),
            (None, Some(path)) => Some(
                tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("failed to read setup token file `{}`", path.display()))?
                    .trim()
                    .to_owned(),
            ),
            (None, None) => None,
            _ => unreachable!("clap enforces setup token conflicts"),
        };
        let setup = bootstrap::SetupServer::prepare(
            store.clone(),
            bootstrap::SetupOptions {
                bind_address: args.setup_bind,
                requested_port: args.setup_port,
                token,
                ..Default::default()
            },
        )?;
        info!("first-run setup URL: https://{}/setup", setup.address);
        info!("first-run setup token: {}", setup.token);
        info!(
            "first-run certificate SHA-256 fingerprint: {}",
            setup.certificate_fingerprint_sha256
        );
        setup.run().await?;
    } else {
        if args.setup_port.is_some() || args.setup_token.is_some() || args.setup_token_file.is_some() {
            info!("runtime is already initialized; setup port/token options are ignored");
        }
    }
    let stored = store
        .load_config_async()
        .await?
        .context("runtime database has no configuration after setup")?;
    runtime::run(&args.runtime_dir, store, stored).await
}

async fn migrate(args: MigrateArgs) -> Result<()> {
    let store = store::Store::open(&args.runtime_dir)?;
    if store.is_initialized()? {
        anyhow::bail!("runtime database is already initialized; refusing to overwrite it");
    }
    let legacy = config::Config::load_file(&args.config)
        .await
        .map_err(|cause| anyhow::anyhow!("failed to load `{}`: {cause}", args.config.display()))?;
    let password = tokio::fs::read_to_string(&args.admin_password_file)
        .await
        .with_context(|| format!("failed to read `{}`", args.admin_password_file.display()))?;
    let administrator = auth::UserRecord {
        username: args.admin_username,
        password_hash: auth::hash_password(password.trim())?,
        administrator: true,
        created_at: Some(time::OffsetDateTime::now_utc()),
        disabled: false,
    };
    let mut runtime_config = runtime_config::RuntimeConfig::from_legacy(legacy);
    runtime_config.control_plane.hostname = store::normalize_domain(&args.control_hostname)?;
    runtime_config.control_plane.self_ips = args.self_ips.into_iter().map(|ip| ip.to_string()).collect();
    runtime_config.control_plane.provider_id = args.provider_id;
    store.bootstrap(&runtime_config, &administrator)?;
    println!("migrated {} into {}", args.config.display(), args.runtime_dir.display());
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn run_accepts_port_alias_and_runtime_directory() {
        let cli = Cli::try_parse_from([
            "tlsproxy",
            "run",
            "--port",
            "44448",
            "--runtime-dir",
            "/tmp/tlsproxy-test",
        ])
        .unwrap();
        let Some(Command::Run(args)) = cli.command else { panic!("expected run") };
        assert_eq!(args.setup_port, Some(44448));
        assert_eq!(args.runtime_dir, PathBuf::from("/tmp/tlsproxy-test"));
    }

    #[test]
    fn genconfig_and_validate_are_no_longer_commands() {
        assert!(Cli::try_parse_from(["tlsproxy", "genconfig"]).is_err());
        assert!(Cli::try_parse_from(["tlsproxy", "validate"]).is_err());
    }

    #[tokio::test]
    async fn explicit_migration_bootstraps_control_plane_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("legacy.yaml");
        let password_path = directory.path().join("password");
        let runtime_dir = directory.path().join("runtime");
        tokio::fs::write(
            &config_path,
            serde_yaml_ng::to_string(&config::Config::default()).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(&password_path, "correct horse battery staple\n")
            .await
            .unwrap();
        migrate(MigrateArgs {
            config: config_path,
            runtime_dir: runtime_dir.clone(),
            admin_username: "admin".into(),
            admin_password_file: password_path,
            control_hostname: "TLS.Example.".into(),
            self_ips: vec!["192.0.2.10".parse().unwrap()],
            provider_id: "letsencrypt-staging".into(),
        })
        .await
        .unwrap();
        let store = store::Store::open(runtime_dir).unwrap();
        let stored = store.load_config().unwrap().unwrap();
        assert_eq!(stored.config.control_plane.hostname, "tls.example");
        assert!(store.user("admin").unwrap().is_some());
        assert!(store.certificate_for_domain("tls.example").unwrap().is_some());
    }

    #[test]
    fn checkpoint_backup_and_empty_directory_restore_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("runtime");
        let checkpoint = directory.path().join("checkpoint");
        let restored = directory.path().join("restored");
        let store = store::Store::open(&runtime).unwrap();
        store.save_config(&runtime_config::RuntimeConfig::default(), "test").unwrap();
        drop(store);
        backup(BackupArgs { runtime_dir: runtime, output: checkpoint.clone() }).unwrap();
        restore(RestoreArgs { checkpoint, runtime_dir: restored.clone() }).unwrap();
        assert_eq!(store::Store::open(restored).unwrap().load_config().unwrap().unwrap().revision, 1);
    }
}
