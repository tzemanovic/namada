use std::ffi::OsStr;
use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, thread, time};

use assert_cmd::assert::OutputAssertExt;
use color_eyre::eyre::Result;
use color_eyre::owo_colors::OwoColorize;
use expectrl::process::unix::{PtyStream, UnixProcess};
use expectrl::session::Session;
use expectrl::stream::log::LoggedStream;
use expectrl::{Eof, WaitStatus};
use eyre::eyre;
use itertools::{Either, Itertools};
use namada::types::chain::ChainId;
use namada_apps::client::utils;
use namada_apps::config::genesis::genesis_config::{self, GenesisConfig};
use namada_apps::{config, wallet};
use rand::Rng;
use tempfile::{tempdir, TempDir};

use crate::e2e::helpers::generate_bin_command;

/// For `color_eyre::install`, which fails if called more than once in the same
/// process
pub static INIT: Once = Once::new();

pub const APPS_PACKAGE: &str = "namada_apps";

/// Env. var for running E2E tests in debug mode
pub const ENV_VAR_DEBUG: &str = "ANOMA_E2E_DEBUG";

/// Env. var for keeping temporary files created by the E2E tests
const ENV_VAR_KEEP_TEMP: &str = "ANOMA_E2E_KEEP_TEMP";

/// Env. var to use a set of prebuilt binaries. This variable holds the path to
/// a folder.
pub const ENV_VAR_USE_PREBUILT_BINARIES: &str =
    "ANOMA_E2E_USE_PREBUILT_BINARIES";

/// The E2E tests genesis config source.
/// This file must contain a single validator with alias "validator-0".
/// To add more validators, use the [`add_validators`] function in the call to
/// setup the [`network`].
pub const SINGLE_NODE_NET_GENESIS: &str = "genesis/e2e-tests-single-node.toml";
/// An E2E test network.
#[derive(Debug)]
pub struct Network {
    pub chain_id: ChainId,
}

/// Add `num` validators to the genesis config. Note that called from inside
/// the [`network`]'s first argument's closure, there is 1 validator already
/// present to begin with, so e.g. `add_validators(1, _)` will configure a
/// network with 2 validators.
///
/// INVARIANT: Do not call this function more than once on the same config.
pub fn add_validators(num: u8, mut genesis: GenesisConfig) -> GenesisConfig {
    let validator_0 = genesis.validator.get_mut("validator-0").unwrap();
    // Clone the first validator before modifying it
    let other_validators = validator_0.clone();
    // Set the first validator to be a bootstrap node to enable P2P connectivity
    validator_0.intent_gossip_seed = Some(true);
    // A bootstrap node doesn't participate in the gossipsub protocol for
    // gossiping intents, so we remove its matchmaker
    validator_0.matchmaker_account = None;
    validator_0.matchmaker_code = None;
    validator_0.matchmaker_tx = None;
    let net_address_0 =
        SocketAddr::from_str(validator_0.net_address.as_ref().unwrap())
            .unwrap();
    let net_address_port_0 = net_address_0.port();
    for ix in 0..num {
        let mut validator = other_validators.clone();
        // Only the first validator is bootstrap
        validator.intent_gossip_seed = None;
        let mut net_address = net_address_0;
        // 6 ports for each validator
        let first_port = net_address_port_0 + 6 * (ix as u16 + 1);
        net_address.set_port(first_port);
        validator.net_address = Some(net_address.to_string());
        let name = format!("validator-{}", ix + 1);
        genesis.validator.insert(name, validator);
    }
    genesis
}

/// Setup a network with a single genesis validator node.
pub fn single_node_net() -> Result<Test> {
    network(|genesis| genesis, None)
}

pub fn network(
    update_genesis: impl Fn(GenesisConfig) -> GenesisConfig,
    consensus_timeout_commit: Option<&'static str>,
) -> Result<Test> {
    INIT.call_once(|| {
        if let Err(err) = color_eyre::install() {
            eprintln!("Failed setting up colorful error reports {}", err);
        }
    });

    let working_dir = working_dir();
    let test_dir = TestDir::new();

    // Open the source genesis file
    let genesis = genesis_config::open_genesis_config(
        working_dir.join(SINGLE_NODE_NET_GENESIS),
    );

    // Run the provided function on it
    let genesis = update_genesis(genesis);

    // Run `init-network` to generate the finalized genesis config, keys and
    // addresses and update WASM checksums
    let genesis_file = test_dir.path().join("e2e-test-genesis-src.toml");
    genesis_config::write_genesis_config(&genesis, &genesis_file);
    let genesis_path = genesis_file.to_string_lossy();
    let checksums_path = working_dir
        .join("wasm/checksums.json")
        .to_string_lossy()
        .into_owned();
    let mut args = vec![
        "utils",
        "init-network",
        "--unsafe-dont-encrypt",
        "--genesis-path",
        &genesis_path,
        "--chain-prefix",
        "e2e-test",
        "--localhost",
        "--dont-archive",
        "--allow-duplicate-ip",
        "--wasm-checksums-path",
        &checksums_path,
    ];
    if let Some(consensus_timeout_commit) = consensus_timeout_commit {
        args.push("--consensus-timeout-commit");
        args.push(consensus_timeout_commit)
    }
    let mut init_network = run_cmd(
        Bin::Client,
        args,
        Some(5),
        &working_dir,
        &test_dir,
        "validator",
        format!("{}:{}", std::file!(), std::line!()),
    )?;

    // Get the generated chain_id` from result of the last command
    let (unread, matched) =
        init_network.exp_regex(r"Derived chain ID: .*\n")?;
    let chain_id_raw =
        matched.trim().split_once("Derived chain ID: ").unwrap().1;
    let chain_id = ChainId::from_str(chain_id_raw.trim())?;
    println!("'init-network' output: {}", unread);
    let net = Network { chain_id };

    // Move the "others" accounts wallet in the main base dir, so that we can
    // use them with `Who::NonValidator`
    let chain_dir = test_dir.path().join(net.chain_id.as_str());
    std::fs::rename(
        wallet::wallet_file(
            chain_dir
                .join(utils::NET_ACCOUNTS_DIR)
                .join(utils::NET_OTHER_ACCOUNTS_DIR),
        ),
        wallet::wallet_file(chain_dir.clone()),
    )
    .unwrap();

    copy_wasm_to_chain_dir(
        &working_dir,
        &chain_dir,
        &net.chain_id,
        genesis.validator.keys(),
    );

    Ok(Test {
        working_dir,
        test_dir,
        net,
        genesis,
    })
}

/// Anoma binaries
#[derive(Debug)]
pub enum Bin {
    Node,
    Client,
    Wallet,
}

#[derive(Debug)]
pub struct Test {
    /// The dir where the tests run from, usually the repo root dir
    pub working_dir: PathBuf,
    /// Temporary test directory is used as the default base-dir for running
    /// Anoma cmds
    pub test_dir: TestDir,
    pub net: Network,
    pub genesis: GenesisConfig,
}

#[derive(Debug)]
pub struct TestDir(Either<TempDir, PathBuf>);

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        match &self.0 {
            Either::Left(temp_dir) => temp_dir.path(),
            Either::Right(path) => path.as_ref(),
        }
    }
}

impl TestDir {
    /// Setup a `TestDir` in a temporary directory. The directory will be
    /// automatically deleted after the test run, unless `ENV_VAR_KEEP_TEMP`
    /// is set to `true`.
    pub fn new() -> Self {
        let keep_temp = match env::var(ENV_VAR_KEEP_TEMP) {
            Ok(val) => val.to_ascii_lowercase() != "false",
            _ => false,
        };

        if keep_temp {
            let path = tempdir().unwrap().into_path();
            println!(
                "{}: \"{}\"",
                "Keeping test directory at".underline().yellow(),
                path.to_string_lossy()
            );
            Self(Either::Right(path))
        } else {
            Self(Either::Left(tempdir().unwrap()))
        }
    }

    /// Get the [`Path`] to the test directory.
    pub fn path(&self) -> &Path {
        self.as_ref()
    }
}

impl Drop for Test {
    fn drop(&mut self) {
        if let Either::Right(path) = &self.test_dir.0 {
            println!(
                "{}: \"{}\"",
                "Keeping test directory at".underline().yellow(),
                path.to_string_lossy()
            );
        }
    }
}

// Internally used macros only for attaching source locations to commands
#[macro_use]
mod macros {
    /// Get an [`AnomaCmd`] to run an Anoma binary. By default, these will run
    /// in release mode. This can be disabled by setting environment
    /// variable `ANOMA_E2E_DEBUG=true`.
    /// On [`AnomaCmd`], you can then call e.g. `exp_string` or `exp_regex` to
    /// look for an expected output from the command.
    ///
    /// Arguments:
    /// - the test [`super::Test`]
    /// - which binary to run [`super::Bin`]
    /// - arguments, which implement `IntoIterator<Item = &str>`, e.g.
    ///   `&["cmd"]`
    /// - optional timeout in seconds `Option<u64>`
    ///
    /// This is a helper macro that adds file and line location to the
    /// [`super::run_cmd`] function call.
    #[macro_export]
    macro_rules! run {
        ($test:expr, $bin:expr, $args:expr, $timeout_sec:expr $(,)?) => {{
            // The file and line will expand to the location that invoked
            // `run_cmd!`
            let loc = format!("{}:{}", std::file!(), std::line!());
            $test.run_cmd($bin, $args, $timeout_sec, loc)
        }};
    }

    /// Get an [`AnomaCmd`] to run an Anoma binary. By default, these will run
    /// in release mode. This can be disabled by setting environment
    /// variable `ANOMA_E2E_DEBUG=true`.
    /// On [`AnomaCmd`], you can then call e.g. `exp_string` or `exp_regex` to
    /// look for an expected output from the command.
    ///
    /// Arguments:
    /// - the test [`super::Test`]
    /// - who to run this command as [`super::Who`]
    /// - which binary to run [`super::Bin`]
    /// - arguments, which implement `IntoIterator<item = &str>`, e.g.
    ///   `&["cmd"]`
    /// - optional timeout in seconds `Option<u64>`
    ///
    /// This is a helper macro that adds file and line location to the
    /// [`super::run_cmd`] function call.
    #[macro_export]
    macro_rules! run_as {
        (
            $test:expr,
            $who:expr,
            $bin:expr,
            $args:expr,
            $timeout_sec:expr $(,)?
        ) => {{
            // The file and line will expand to the location that invoked
            // `run_cmd!`
            let loc = format!("{}:{}", std::file!(), std::line!());
            $test.run_cmd_as($who, $bin, $args, $timeout_sec, loc)
        }};
    }
}

pub enum Who {
    // A non-validator
    NonValidator,
    // Genesis validator with a given index, starting from `0`
    Validator(u64),
}

impl Test {
    /// Use the `run!` macro instead of calling this method directly to get
    /// automatic source location reporting.
    ///
    /// Get an [`AnomaCmd`] to run an Anoma binary. By default, these will run
    /// in release mode. This can be disabled by setting environment
    /// variable `ANOMA_E2E_DEBUG=true`.
    pub fn run_cmd<I, S>(
        &self,
        bin: Bin,
        args: I,
        timeout_sec: Option<u64>,
        loc: String,
    ) -> Result<AnomaCmd>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_cmd_as(Who::NonValidator, bin, args, timeout_sec, loc)
    }

    /// Use the `run!` macro instead of calling this method directly to get
    /// automatic source location reporting.
    ///
    /// Get an [`AnomaCmd`] to run an Anoma binary. By default, these will run
    /// in release mode. This can be disabled by setting environment
    /// variable `ANOMA_E2E_DEBUG=true`.
    pub fn run_cmd_as<I, S>(
        &self,
        who: Who,
        bin: Bin,
        args: I,
        timeout_sec: Option<u64>,
        loc: String,
    ) -> Result<AnomaCmd>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let base_dir = self.get_base_dir(&who);
        let mode = match &who {
            Who::NonValidator => "full",
            Who::Validator(_) => "validator",
        };
        run_cmd(
            bin,
            args,
            timeout_sec,
            &self.working_dir,
            &base_dir,
            mode,
            loc,
        )
    }

    pub fn get_base_dir(&self, who: &Who) -> PathBuf {
        match who {
            Who::NonValidator => self.test_dir.path().to_owned(),
            Who::Validator(index) => self
                .test_dir
                .path()
                .join(self.net.chain_id.as_str())
                .join(utils::NET_ACCOUNTS_DIR)
                .join(format!("validator-{}", index))
                .join(config::DEFAULT_BASE_DIR),
        }
    }
}

/// A helper that should be ran on start of every e2e test case.
pub fn working_dir() -> PathBuf {
    let working_dir = fs::canonicalize("..").unwrap();

    // Check that tendermint is either on $PATH or `TENDERMINT` env var is set
    if std::env::var("TENDERMINT").is_err() {
        Command::new("which")
            .arg("tendermint")
            .assert()
            .try_success()
            .expect(
                "The env variable TENDERMINT must be set and point to a local \
                 build of the tendermint abci++ branch, or the tendermint \
                 binary must be on PATH",
            );
    }
    working_dir
}

/// A command under test
pub struct AnomaCmd {
    pub session: Session<UnixProcess, LoggedStream<PtyStream, File>>,
    pub cmd_str: String,
    pub log_path: PathBuf,
}

impl Display for AnomaCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\nLogs: {}",
            self.cmd_str,
            self.log_path.to_string_lossy()
        )
    }
}

/// A command under test running on a background thread
pub struct AnomaBgCmd {
    join_handle: std::thread::JoinHandle<AnomaCmd>,
    abort_send: std::sync::mpsc::Sender<()>,
}

impl AnomaBgCmd {
    /// Re-gain control of a background command (created with
    /// [`AnomaCmd::background()`]) to check its output.
    pub fn foreground(self) -> AnomaCmd {
        self.abort_send.send(()).unwrap();
        self.join_handle.join().unwrap()
    }
}

impl AnomaCmd {
    /// Keep reading the session's output in a background thread to prevent the
    /// buffer from filling up. Call [`AnomaBgCmd::foreground()`] on the
    /// returned [`AnomaBgCmd`] to stop the loop and return back the original
    /// command.
    pub fn background(self) -> AnomaBgCmd {
        let (abort_send, abort_recv) = std::sync::mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            let mut cmd = self;
            loop {
                match abort_recv.try_recv() {
                    Ok(())
                    | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return cmd;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                cmd.session.is_matched(Eof).unwrap();
            }
        });
        AnomaBgCmd {
            join_handle,
            abort_send,
        }
    }

    /// Assert that the process exited with success
    pub fn assert_success(&mut self) {
        // Make sure that there is no unread output first
        let _ = self.exp_eof().unwrap();

        let status = self.session.wait().unwrap();
        assert_eq!(WaitStatus::Exited(self.session.pid(), 0), status);
    }

    /// Assert that the process exited with failure
    #[allow(dead_code)]
    pub fn assert_failure(&mut self) {
        // Make sure that there is no unread output first
        let _ = self.exp_eof().unwrap();

        let status = self.session.wait().unwrap();
        assert_ne!(WaitStatus::Exited(self.session.pid(), 0), status);
    }

    /// Wait until provided string is seen on stdout of child process.
    /// Return the yet unread output (without the matched string)
    ///
    /// Wrapper over the inner `PtySession`'s functions with custom error
    /// reporting.
    pub fn exp_string(&mut self, needle: &str) -> Result<String> {
        let found = self.session.expect_eager(needle).map_err(|e| {
            eyre!("{}\nCommand: {}\n Needle: {}", e, self, needle)
        })?;
        if found.is_empty() {
            Err(eyre!(
                "Expected needle not found\nCommand: {}\n Needle: {}",
                self,
                needle
            ))
        } else {
            String::from_utf8(found.before().to_vec())
                .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))
        }
    }

    /// Wait until provided regex is seen on stdout of child process.
    /// Return a tuple:
    /// 1. the yet unread output
    /// 2. the matched regex
    ///
    /// Wrapper over the inner `Session`'s functions with custom error
    /// reporting as well as converting bytes back to `String`.
    pub fn exp_regex(&mut self, regex: &str) -> Result<(String, String)> {
        let found = self
            .session
            .expect_eager(expectrl::Regex(regex))
            .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))?;
        if found.is_empty() {
            Err(eyre!(
                "Expected regex not found: {}\nCommand: {}",
                regex,
                self
            ))
        } else {
            let unread = String::from_utf8(found.before().to_vec())
                .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))?;
            let matched =
                String::from_utf8(found.matches().next().unwrap().to_vec())
                    .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))?;
            Ok((unread, matched))
        }
    }

    /// Wait until we see EOF (i.e. child process has terminated)
    /// Return all the yet unread output
    ///
    /// Wrapper over the inner `Session`'s functions with custom error
    /// reporting.
    #[allow(dead_code)]
    pub fn exp_eof(&mut self) -> Result<String> {
        let found = self
            .session
            .expect_eager(Eof)
            .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))?;
        if found.is_empty() {
            Err(eyre!("Expected EOF\nCommand: {}", self))
        } else {
            String::from_utf8(found.before().to_vec())
                .map_err(|e| eyre!(format!("Error: {}\nCommand: {}", e, self)))
        }
    }

    /// Send a control code to the running process and consume resulting output
    /// line (which is empty because echo is off)
    ///
    /// E.g. `send_control('c')` sends ctrl-c. Upper/smaller case does not
    /// matter.
    ///
    /// Wrapper over the inner `Session`'s functions with custom error
    /// reporting.
    pub fn send_control(&mut self, c: char) -> Result<()> {
        self.session
            .send_control(c)
            .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))
    }

    /// send line to repl (and flush output) and then, if echo_on=true wait for
    /// the input to appear.
    /// Return: number of bytes written
    ///
    /// Wrapper over the inner `Session`'s functions with custom error
    /// reporting.
    pub fn send_line(&mut self, line: &str) -> Result<()> {
        self.session
            .send_line(line)
            .map_err(|e| eyre!("Error: {}\nCommand: {}", e, self))
    }
}

impl Drop for AnomaCmd {
    fn drop(&mut self) {
        // attempt to clean up the process
        println!(
            "{}: {}",
            "> Sending Ctrl+C to command".underline().yellow(),
            self.cmd_str,
        );
        let _result = self.send_control('c');
        match self.exp_eof() {
            Err(error) => {
                eprintln!(
                    "\n{}: {}\n{}: {}",
                    "> Error ensuring command is finished".underline().red(),
                    self.cmd_str,
                    "Error".underline().red(),
                    error,
                );
            }
            Ok(output) => {
                println!(
                    "\n{}: {}",
                    "> Command finished".underline().green(),
                    self.cmd_str,
                );
                let output = output.trim();
                if !output.is_empty() {
                    println!(
                        "\n{}: {}\n\n{}",
                        "> Unread output for command".underline().yellow(),
                        self.cmd_str,
                        output
                    );
                } else {
                    println!(
                        "\n{}: {}",
                        "> No unread output for command".underline().green(),
                        self.cmd_str
                    );
                }
            }
        }
    }
}

/// Get a [`Command`] to run an Anoma binary. By default, these will run in
/// release mode. This can be disabled by setting environment variable
/// `ANOMA_E2E_DEBUG=true`.
pub fn run_cmd<I, S>(
    bin: Bin,
    args: I,
    timeout_sec: Option<u64>,
    working_dir: impl AsRef<Path>,
    base_dir: impl AsRef<Path>,
    mode: &str,
    loc: String,
) -> Result<AnomaCmd>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Root cargo workspace manifest path
    let bin_name = match bin {
        Bin::Node => "namadan",
        Bin::Client => "namadac",
        Bin::Wallet => "namadaw",
    };

    let mut run_cmd = generate_bin_command(
        bin_name,
        &working_dir.as_ref().join("Cargo.toml"),
    );

    run_cmd
        .env("ANOMA_LOG", "info")
        .env("TM_LOG_LEVEL", "info")
        .env("ANOMA_LOG_COLOR", "false")
        .current_dir(working_dir)
        .args(&[
            "--base-dir",
            &base_dir.as_ref().to_string_lossy(),
            "--mode",
            mode,
        ])
        .args(args);

    let args: String =
        run_cmd.get_args().map(|s| s.to_string_lossy()).join(" ");
    let cmd_str =
        format!("{} {}", run_cmd.get_program().to_string_lossy(), args);

    let session = Session::spawn(run_cmd).map_err(|e| {
        eyre!(
            "\n\n{}: {}\n{}: {}\n{}: {}",
            "Failed to run".underline().red(),
            cmd_str,
            "Location".underline().red(),
            loc,
            "Error".underline().red(),
            e
        )
    })?;

    let log_path = {
        let mut rng = rand::thread_rng();
        let log_dir = base_dir.as_ref().join("logs");
        fs::create_dir_all(&log_dir)?;
        log_dir.join(&format!(
            "{}-{}-{}.log",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros(),
            bin_name,
            rng.gen::<u64>()
        ))
    };
    let logger = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_path)?;
    let mut session = session.with_log(logger).unwrap();

    session.set_expect_timeout(timeout_sec.map(std::time::Duration::from_secs));

    let mut cmd_process = AnomaCmd {
        session,
        cmd_str,
        log_path,
    };

    println!("{}:\n{}", "> Running".underline().green(), &cmd_process);

    if let Bin::Node = &bin {
        // When running a node command, we need to wait a bit before checking
        // status
        sleep(1);

        // If the command failed, try print out its output
        if let Ok(WaitStatus::Exited(_, result)) = cmd_process.session.status()
        {
            if result != 0 {
                let output = cmd_process.exp_eof().unwrap_or_else(|err| {
                    format!("No output found, error: {}", err)
                });
                return Err(eyre!(
                    "\n\n{}: {}\n{}: {} \n\n{}: {}",
                    "Failed to run".underline().red(),
                    cmd_process.cmd_str,
                    "Location".underline().red(),
                    loc,
                    "Output".underline().red(),
                    output,
                ));
            }
        }
    }

    Ok(cmd_process)
}

/// Sleep for given `seconds`.
pub fn sleep(seconds: u64) {
    thread::sleep(time::Duration::from_secs(seconds));
}

#[allow(dead_code)]
pub mod constants {
    use std::fs;
    use std::path::PathBuf;

    // User addresses aliases
    pub const ALBERT: &str = "Albert";
    pub const ALBERT_KEY: &str = "Albert-key";
    pub const BERTHA: &str = "Bertha";
    pub const BERTHA_KEY: &str = "Bertha-key";
    pub const CHRISTEL: &str = "Christel";
    pub const CHRISTEL_KEY: &str = "Christel-key";
    pub const DAEWON: &str = "Daewon";
    pub const MATCHMAKER_KEY: &str = "matchmaker-key";

    //  Native VP aliases
    pub const GOVERNANCE_ADDRESS: &str = "governance";

    // Fungible token addresses
    pub const XAN: &str = "XAN";
    pub const BTC: &str = "BTC";
    pub const ETH: &str = "ETH";
    pub const DOT: &str = "DOT";

    // Bite-sized tokens
    pub const SCHNITZEL: &str = "Schnitzel";
    pub const APFEL: &str = "Apfel";
    pub const KARTOFFEL: &str = "Kartoffel";

    // Paths to the WASMs used for tests
    pub const TX_TRANSFER_WASM: &str = "wasm/tx_transfer.wasm";
    pub const VP_USER_WASM: &str = "wasm/vp_user.wasm";
    pub const TX_NO_OP_WASM: &str = "wasm_for_tests/tx_no_op.wasm";
    pub const TX_INIT_PROPOSAL: &str = "wasm_for_tests/tx_init_proposal.wasm";
    pub const TX_WRITE_STORAGE_KEY_WASM: &str =
        "wasm_for_tests/tx_write_storage_key.wasm";
    pub const VP_ALWAYS_TRUE_WASM: &str = "wasm_for_tests/vp_always_true.wasm";
    pub const VP_ALWAYS_FALSE_WASM: &str =
        "wasm_for_tests/vp_always_false.wasm";
    pub const TX_MINT_TOKENS_WASM: &str = "wasm_for_tests/tx_mint_tokens.wasm";
    pub const TX_PROPOSAL_CODE: &str = "wasm_for_tests/tx_proposal_code.wasm";

    /// Find the absolute path to one of the WASM files above
    pub fn wasm_abs_path(file_name: &str) -> PathBuf {
        let working_dir = fs::canonicalize("..").unwrap();
        working_dir.join(file_name)
    }
}

/// Copy WASM files from the `wasm` directory to every node's chain dir.
pub fn copy_wasm_to_chain_dir<'a>(
    working_dir: &Path,
    chain_dir: &Path,
    chain_id: &ChainId,
    genesis_validator_keys: impl Iterator<Item = &'a String>,
) {
    // Copy the built WASM files from "wasm" directory in the root of the
    // project.
    let built_wasm_dir = working_dir.join(config::DEFAULT_WASM_DIR);
    let opts = fs_extra::dir::DirOptions { depth: 1 };
    let wasm_files: Vec<_> =
        fs_extra::dir::get_dir_content2(&built_wasm_dir, &opts)
            .unwrap()
            .files
            .into_iter()
            .map(PathBuf::from)
            .filter(|path| {
                matches!(path.extension().and_then(OsStr::to_str), Some("wasm"))
            })
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
    if wasm_files.is_empty() {
        panic!(
            "No WASM files found in {}. Please build or download them them \
             first.",
            built_wasm_dir.to_string_lossy()
        );
    }
    let target_wasm_dir = chain_dir.join(config::DEFAULT_WASM_DIR);
    for file in &wasm_files {
        std::fs::copy(
            working_dir.join("wasm").join(&file),
            target_wasm_dir.join(&file),
        )
        .unwrap();
    }

    // Copy the built WASM files from "wasm" directory to each validator dir
    for validator_name in genesis_validator_keys {
        let target_wasm_dir = chain_dir
            .join(utils::NET_ACCOUNTS_DIR)
            .join(validator_name)
            .join(config::DEFAULT_BASE_DIR)
            .join(chain_id.as_str())
            .join(config::DEFAULT_WASM_DIR);
        for file in &wasm_files {
            std::fs::copy(
                working_dir.join("wasm").join(&file),
                target_wasm_dir.join(&file),
            )
            .unwrap();
        }
    }
}
