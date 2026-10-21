//! Record, check and compare the parity snapshot corpus.
//!
//! Three verbs, and only the first needs Obsidian:
//!
//!   record   ask the LIVE CLI what it answers, and store the answers
//!   check    validate a stored corpus, with no CLI and no vault
//!   compare  resolve the vault ourselves and compare against the store
//!
//! `just record`, `just corpus-check` and `just parity-recorded` are the
//! supported entry points; this binary is what they run. The reasoning behind
//! the store, and the guards around recording, live in [`bases_mcp::parity`].

use std::path::PathBuf;
use std::process::ExitCode;

use bases_mcp::parity::{self, Cli, RecordRequest};
use bases_mcp::service::Resolver;

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("parity-record: {message}");
            eprintln!();
            eprintln!("{}", usage());
            return ExitCode::FAILURE;
        }
    };
    match run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("parity-record: {message}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Record,
    Check,
    Compare,
    Help,
}

#[derive(Debug)]
struct Options {
    command: Command,
    vault: String,
    vault_dir: PathBuf,
    data_dir: PathBuf,
    corpus: Option<PathBuf>,
}

fn defaults() -> Options {
    Options {
        command: Command::Help,
        vault: std::env::var("OBSIDIAN_VAULT").unwrap_or_else(|_| "vault".into()),
        vault_dir: PathBuf::from("test/vault"),
        data_dir: PathBuf::from("test/parity/data"),
        corpus: None,
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let first = args.next().ok_or("no command given")?;
    let command = match first.as_str() {
        "record" => Command::Record,
        "check" => Command::Check,
        "compare" => Command::Compare,
        "-h" | "--help" | "help" => Command::Help,
        other => return Err(format!("unknown command `{other}`")),
    };
    let mut options = Options {
        command,
        ..defaults()
    };

    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        match flag.as_str() {
            "-h" | "--help" => options.command = Command::Help,
            "--vault" => options.vault = value(&mut args, inline, "--vault")?,
            "--vault-dir" => {
                options.vault_dir = PathBuf::from(value(&mut args, inline, "--vault-dir")?)
            }
            "--data-dir" => {
                options.data_dir = PathBuf::from(value(&mut args, inline, "--data-dir")?)
            }
            "--corpus" => {
                options.corpus = Some(PathBuf::from(value(&mut args, inline, "--corpus")?))
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }

    match options.command {
        Command::Record if options.corpus.is_some() => {
            Err("`record` does not take --corpus; it creates one".into())
        }
        Command::Check if options.corpus.is_some() => {
            Err("`check` validates every corpus under --data-dir, not one --corpus".into())
        }
        _ => Ok(options),
    }
}

fn value(
    args: &mut impl Iterator<Item = String>,
    inline: Option<String>,
    flag: &str,
) -> Result<String, String> {
    match inline {
        Some(value) if !value.is_empty() => Ok(value),
        Some(_) => Err(format!("{flag} needs a value")),
        None => args.next().ok_or_else(|| format!("{flag} needs a value")),
    }
}

fn run(options: Options) -> Result<(), String> {
    match options.command {
        Command::Help => {
            println!("{}", usage());
            Ok(())
        }
        Command::Record => {
            let cli = Cli::new(
                std::env::var("OBSIDIAN_BIN").unwrap_or_else(|_| "obsidian".into()),
                options.vault,
            );
            eprintln!(
                "parity-record: recording vault `{}` from {} with {}",
                cli.vault,
                options.vault_dir.display(),
                cli.binary
            );
            let request = RecordRequest {
                cli,
                vault_dir: options.vault_dir,
                data_dir: options.data_dir,
            };
            let outcome = runtime()?
                .block_on(parity::record(&request))
                .map_err(fail)?;
            if outcome.already_present {
                println!(
                    "parity-record: {} entries already recorded as {}",
                    outcome.entries,
                    outcome.dir.display()
                );
            } else {
                println!(
                    "parity-record: recorded {} entries as {}",
                    outcome.entries,
                    outcome.dir.display()
                );
                println!(
                    "parity-record: the key is the digest of the answers, not the CLI version: \
an Obsidian version that answers identically reuses this directory"
                );
            }
            Ok(())
        }
        Command::Check => match parity::check_root(&options.data_dir) {
            Ok(corpora) => {
                let entries: usize = corpora.iter().map(|corpus| corpus.entries.len()).sum();
                for corpus in &corpora {
                    println!(
                        "corpus {}: {} entries, recorded with {} {} (vault `{}`)",
                        corpus.manifest.digest,
                        corpus.entries.len(),
                        corpus.manifest.recorded_with.cli,
                        corpus.manifest.recorded_with.version,
                        corpus.manifest.recorded_with.vault
                    );
                }
                println!(
                    "parity-record: {} corpora, {entries} entries — OK",
                    corpora.len()
                );
                Ok(())
            }
            Err(mut errors) => {
                if errors.len() == 1 {
                    return Err(errors.remove(0));
                }
                for error in &errors {
                    eprintln!("parity-record: {error}");
                }
                Err(format!("{} problems in the corpus", errors.len()))
            }
        },
        Command::Compare => {
            let corpus = match options.corpus {
                Some(path) => path,
                None => parity::sole_corpus(&options.data_dir).map_err(fail)?,
            };
            let corpus = parity::load_corpus(&corpus).map_err(fail)?;
            let resolver = runtime()?
                .block_on(Resolver::open_dir(&options.vault_dir))
                .map_err(fail)?;
            let report = runtime()?
                .block_on(parity::compare(&resolver, &corpus))
                .map_err(fail)?;
            for line in &report.skipped {
                println!("skip  {line}");
            }
            for line in &report.failures {
                eprintln!("FAIL  {line}");
            }
            println!(
                "parity-record: corpus {}, {} entries compared, {} skipped, {} failed",
                corpus.dir.display(),
                report.compared,
                report.skipped.len(),
                report.failures.len()
            );
            if report.ok() {
                Ok(())
            } else {
                Err("the recorded corpus does not match this implementation".into())
            }
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start a tokio runtime: {e}"))
}

fn fail(error: bases_mcp::error::BasesError) -> String {
    error.display_message()
}

fn usage() -> &'static str {
    "parity-record — record, check and compare the parity snapshot corpus

  parity-record record  [--vault NAME] [--vault-dir DIR] [--data-dir DIR]
  parity-record check   [--data-dir DIR]
  parity-record compare [--vault-dir DIR] [--data-dir DIR] [--corpus DIR]

record   asks the LIVE Obsidian CLI for every base view and stores the answers.
         Needs the desktop app running with the vault registered; it refuses
         unless `vault=<name>` is registered at --vault-dir, and writes nothing
         if the app rewrote a single byte of the vault.
check    validates the stored corpus: manifest, digests, entry files, no strays.
         No CLI and no vault needed.
compare  resolves the vault ourselves and compares every answer with the store,
         naming the row and column that differ. No CLI needed.

Defaults: --vault vault, --vault-dir test/vault, --data-dir test/parity/data.

Environment:
  OBSIDIAN_BIN         the CLI to run (default: obsidian)
  OBSIDIAN_VAULT       the registered vault name (default: vault)
  OBSIDIAN_CONFIG_DIR  the directory holding obsidian.json"
}
