use crate::backend::Backend;
use clap::{Args, Parser, Subcommand};
use std::fmt::Display;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize NOMT backend for the specified workload.
    ///
    /// The backend will be initialized with all the data required
    /// to execute the workload.
    Init(InitParams),
    /// Execute a workload over the given backend.
    ///
    /// This will not reset the database unless `--reset` is provided.
    Run(RunParams),
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Backend::SovDB => "sov-db",
            Backend::Nomt => "nomt",
            Backend::SpTrie => "sp-trie",
        };
        f.write_str(name)
    }
}

/// Parameters to the init command.
#[derive(Debug, Args)]
pub struct InitParams {
    #[clap(flatten)]
    pub workload: WorkloadParams,

    /// The backend to run the workload against.
    #[arg(required = true, long, short)]
    pub backend: Backend,
}

/// Parameters to the run command.
#[derive(Debug, Args)]
pub struct RunParams {
    #[clap(flatten)]
    pub workload: WorkloadParams,

    #[clap(flatten)]
    pub limits: RunLimits,

    /// The backend to run the workload against.
    #[arg(required = true, long, short)]
    pub backend: Backend,

    /// Whether to reset the database.
    ///
    /// If this is false, no initialization logic will be run and the database is assumed to
    /// be initialized for the workload.
    #[clap(default_value = "false")]
    #[arg(long, short)]
    pub reset: bool,
}

#[derive(Clone, Debug, Args)]
pub struct WorkloadParams {
    /// Workload used by benchmarks.
    ///
    /// Possible values are: transfer, randr, randw, randrw
    ///
    /// `transfer` workload involves balancing transfer between two different accounts.
    ///
    /// `randr` and `randw` will perform randomly uniformly distributed reads and writes,
    /// respectively, over the key space.
    #[clap(default_value = "transfer")]
    #[arg(long = "workload-name", short = 'w')]
    pub name: String,

    /// Parameters available only with workload "transfer".
    ///
    /// It is the percentage of transfers to a non-existing account,
    /// the remaining portion of transfers are to existing accounts
    ///
    /// Accepted values are in the range of 0 to 100
    #[clap(value_parser=clap::value_parser!(u8).range(0..=100))]
    #[arg(long = "workload-percentage-cold", short)]
    pub percentage_cold: Option<u8>,

    /// Amount of operations performed in the workload per iteration.
    #[clap(default_value = "1000")]
    #[arg(long = "workload-size", short)]
    pub size: u64,

    /// The size of the database before starting the benchmarks.
    ///
    /// The provided argument is the power of two exponent of the
    /// number of elements already present in the storage.
    ///
    /// Accepted values are in the range of 0 to 63
    ///
    /// Leave it empty to specify an initial empty storage
    #[arg(long = "workload-capacity", short = 'c')]
    #[clap(value_parser=clap::value_parser!(u8).range(0..64))]
    pub initial_capacity: Option<u8>,

    /// Number of concurrent fetches to perform. Only used with Nomt backend.
    ///
    /// Default value is 1
    #[arg(long = "fetch-concurrency", short)]
    #[clap(default_value = "1")]
    pub fetch_concurrency: usize,
}

#[derive(Debug, Clone, Args)]
#[group(required = true)]
pub struct RunLimits {
    /// The run is limited by having completed this total number of operations.
    #[arg(long = "op-limit")]
    pub ops: Option<u64>,

    /// The run is limited by the given duration.
    #[arg(long = "time-limit")]
    pub time: Option<humantime::Duration>,
}
