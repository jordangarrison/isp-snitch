use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use isp_snitch::config::Config;
use isp_snitch::icmp::Transport;
use isp_snitch::icmp::socket::PingSocket;
use isp_snitch::tracer::TracerCore;
use isp_snitch::tracer::report::trace_table;
use isp_snitch::tracer::runner::{self, RunOpts};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Per-hop ISP packet-loss monitor.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the monitor service (probes, storage, web UI).
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// Validate a config file and print the parsed result.
    CheckConfig {
        #[arg(long)]
        config: PathBuf,
    },
    /// One-shot mtr-style trace, printed as a table.
    Trace {
        /// Device to bind probes to (SO_BINDTODEVICE).
        #[arg(long)]
        interface: Option<String>,
        #[arg(long, default_value_t = 10)]
        rounds: usize,
        #[arg(long, default_value_t = 30)]
        max_ttl: u8,
        addr: Ipv4Addr,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run { config } => run(config),
        Command::CheckConfig { config } => check_config(config),
        Command::Trace {
            interface,
            rounds,
            max_ttl,
            addr,
        } => trace(interface, rounds, max_ttl, addr),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(config: PathBuf) -> Result<()> {
    let cfg = Config::load(&config)?;
    tokio::runtime::Runtime::new()
        .context("starting async runtime")?
        .block_on(isp_snitch::service::run(cfg))
}

fn check_config(config: PathBuf) -> Result<()> {
    let cfg = Config::load(&config)?;
    println!("{cfg:#?}");
    Ok(())
}

fn trace(interface: Option<String>, rounds: usize, max_ttl: u8, addr: Ipv4Addr) -> Result<()> {
    // Open once up front so a bad interface or sysctl fails loudly here.
    PingSocket::open(addr, interface.as_deref())
        .with_context(|| format!("opening ping socket (interface {interface:?})"))?;
    let stop = AtomicBool::new(false);
    let mut done = Vec::new();
    let opts = RunOpts {
        max_rounds: Some(rounds),
        ..RunOpts::new(Duration::from_secs(1), Duration::from_millis(5))
    };
    runner::run(
        || PingSocket::open(addr, interface.as_deref()).map(|s| Box::new(s) as Box<dyn Transport>),
        TracerCore::new(0, max_ttl, Duration::from_secs(2)),
        opts,
        &stop,
        |r| done.push(r),
    );
    print!("{}", trace_table(&done));
    Ok(())
}
