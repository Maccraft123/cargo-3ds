pub mod command;

use core::fmt;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::{env, io, process};

use cargo_metadata::{Message, MetadataCommand};
use command::{Input, Test};
use cytryna::smdh::Smdh;
use rustc_version::Channel;
use semver::Version;
use tee::TeeReader;

use crate::command::{CargoCmd, Run};

/// Build a command using [`make_cargo_build_command`] and execute it,
/// parsing and returning the messages from the spawned process.
///
/// For commands that produce an executable output, this function will build the
/// `.elf` binary that can be used to create other 3ds files.
pub fn run_cargo(input: &Input, message_format: Option<String>) -> (ExitStatus, Vec<Message>) {
    let mut command = make_cargo_command(input, &message_format);

    if input.verbose {
        print_command(&command);
    }

    let mut process = command.spawn().unwrap();
    let command_stdout = process.stdout.take().unwrap();

    let mut tee_reader;
    let mut stdout_reader;

    let buf_reader: &mut dyn BufRead = match (message_format, &input.cmd) {
        // The user presumably cares about the message format if set, so we should
        // copy stuff to stdout like they expect. We can still extract the executable
        // information out of it that we need for 3dsxtool etc.
        (Some(_), _) |
        // Rustdoc unfortunately prints to stdout for compile errors, so
        // we also use a tee when building doc tests too.
        // Possibly related: https://github.com/rust-lang/rust/issues/75135
        (None, CargoCmd::Test(Test { doc: true, .. })) => {
            tee_reader = BufReader::new(TeeReader::new(command_stdout, io::stdout()));
            &mut tee_reader
        }
        _ => {
            stdout_reader = BufReader::new(command_stdout);
            &mut stdout_reader
        }
    };

    let messages = Message::parse_stream(buf_reader)
        .collect::<io::Result<_>>()
        .unwrap();

    (process.wait().unwrap(), messages)
}

/// Create a cargo command based on the context.
///
/// For "build" commands (which compile code, such as `cargo 3ds build` or `cargo 3ds clippy`),
/// if there is no pre-built std detected in the sysroot, `build-std` will be used instead.
pub fn make_cargo_command(input: &Input, message_format: &Option<String>) -> Command {
    let cargo_cmd = &input.cmd;

    let mut command = cargo(&input.config);
    command.arg(cargo_cmd.subcommand_name());

    // Any command that needs to compile code will run under this environment.
    // Even `clippy` and `check` need this kind of context, so we'll just assume any other `Passthrough` command uses it too.
    if cargo_cmd.should_compile() {
        let rust_flags = env::var("RUSTFLAGS").unwrap_or_default()
            + &format!(
                " -L{}/libctru/lib -lctru",
                env::var("DEVKITPRO").expect("DEVKITPRO is not defined as an environment variable")
            );

        command
            .env("RUSTFLAGS", rust_flags)
            .arg("--target")
            .arg("armv6k-nintendo-3ds")
            .arg("--message-format")
            .arg(
                message_format
                    .as_deref()
                    .unwrap_or(CargoCmd::DEFAULT_MESSAGE_FORMAT),
            );

        let sysroot = find_sysroot();
        if !sysroot.join("lib/rustlib/armv6k-nintendo-3ds").exists() {
            eprintln!("No pre-build std found, using build-std");
            // Always building the test crate is not ideal, but we don't know if the
            // crate being built uses #![feature(test)], so we build it just in case.
            command.arg("-Z").arg("build-std=std,test");
        }
    }

    if let CargoCmd::Test(test) = cargo_cmd {
        // RUSTDOCFLAGS is simply ignored if --doc wasn't passed, so we always set it.
        let rustdoc_flags = std::env::var("RUSTDOCFLAGS").unwrap_or_default() + test.rustdocflags();
        command.env("RUSTDOCFLAGS", rustdoc_flags);
    }

    command.args(cargo_cmd.cargo_args());

    if let CargoCmd::Run(run) | CargoCmd::Test(Test { run_args: run, .. }) = &cargo_cmd {
        if run.use_custom_runner() {
            command
                .arg("--")
                .args(run.build_args.passthrough.exe_args());
        }
    }

    command
        .stdout(Stdio::piped())
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());

    command
}

/// Build a `cargo` command with the given `--config` flags.
fn cargo(config: &[String]) -> Command {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(cargo);
    cmd.args(config.iter().map(|cfg| format!("--config={cfg}")));
    cmd
}

fn print_command(command: &Command) {
    let mut cmd_str = vec![command.get_program().to_string_lossy().to_string()];
    cmd_str.extend(command.get_args().map(|s| s.to_string_lossy().to_string()));

    eprintln!("Running command:");
    for (k, v) in command.get_envs() {
        let v = v.map(|v| v.to_string_lossy().to_string());
        eprintln!(
            "   {}={} \\",
            k.to_string_lossy(),
            v.map_or_else(String::new, |s| shlex::quote(&s).to_string())
        );
    }
    eprintln!("   {}\n", shlex::join(cmd_str.iter().map(String::as_str)));
}

/// Finds the sysroot path of the current toolchain
pub fn find_sysroot() -> PathBuf {
    let sysroot = env::var("SYSROOT").ok().unwrap_or_else(|| {
        let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());

        let output = Command::new(&rustc)
            .arg("--print")
            .arg("sysroot")
            .output()
            .unwrap_or_else(|_| panic!("Failed to run `{rustc} -- print sysroot`"));
        String::from_utf8(output.stdout).expect("Failed to parse sysroot path into a UTF-8 string")
    });

    PathBuf::from(sysroot.trim())
}

/// Checks the current rust version and channel.
/// Exits if the minimum requirement is not met.
pub fn check_rust_version() {
    let rustc_version = rustc_version::version_meta().unwrap();

    if rustc_version.channel > Channel::Nightly {
        eprintln!("cargo-3ds requires a nightly rustc version.");
        eprintln!(
            "Please run `rustup override set nightly` to use nightly in the \
            current directory, or use `cargo +nightly 3ds` to use it for a \
            single invocation."
        );
        process::exit(1);
    }

    let old_version = MINIMUM_RUSTC_VERSION
        > Version {
            // Remove `-nightly` pre-release tag for comparison.
            pre: semver::Prerelease::EMPTY,
            ..rustc_version.semver.clone()
        };

    let old_commit = match rustc_version.commit_date {
        None => false,
        Some(date) => {
            MINIMUM_COMMIT_DATE
                > CommitDate::parse(&date).expect("could not parse `rustc --version` commit date")
        }
    };

    if old_version || old_commit {
        eprintln!("cargo-3ds requires rustc nightly version >= {MINIMUM_COMMIT_DATE}");
        eprintln!("Please run `rustup update nightly` to upgrade your nightly version");

        process::exit(1);
    }
}

/// Parses messages returned by "build" cargo commands (such as `cargo 3ds build` or `cargo 3ds run`).
/// The returned [`CTRConfig`] is then used for further building in and execution
/// in [`build_smdh`], [`build_3dsx`], and [`link`].
pub fn get_metadata(messages: &[Message]) -> CTRConfig {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .expect("Failed to get cargo metadata");

    let mut package = None;
    let mut artifact = None;

    // Extract the final built executable. We may want to fail in cases where
    // multiple executables, or none, were built?
    for message in messages.iter().rev() {
        if let Message::CompilerArtifact(art) = message {
            if art.executable.is_some() {
                package = Some(metadata[&art.package_id].clone());
                artifact = Some(art.clone());

                break;
            }
        }
    }
    if package.is_none() || artifact.is_none() {
        eprintln!("No executable found from build command output!");
        process::exit(1);
    }

    let (package, artifact) = (package.unwrap(), artifact.unwrap());

    let mut icon_path = String::from("./icon.png");

    if !Path::new(&icon_path).exists() {
        icon_path = format!(
            "{}/libctru/default_icon.png",
            env::var("DEVKITPRO").unwrap()
        );
    }

    let icon = image::open(Path::new(&icon_path)).expect("Invalid PNG image");

    // for now assume a single "kind" since we only support one output artifact
    let name = match artifact.target.kind[0].as_ref() {
        "bin" | "lib" | "rlib" | "dylib" if artifact.target.test => {
            format!("{} tests", artifact.target.name)
        }
        "example" => {
            format!("{} - {} example", artifact.target.name, package.name)
        }
        _ => artifact.target.name,
    };

    let author = match package.authors.as_slice() {
        [name, ..] => name.clone(),
        [] => String::from("Unspecified Author"), // as standard with the devkitPRO toolchain
    };

    CTRConfig {
        name,
        author,
        description: package
            .description
            .clone()
            .unwrap_or_else(|| String::from("Homebrew Application")),
        icon,
        target_path: artifact.executable.unwrap().into(),
        cargo_manifest_path: package.manifest_path.into(),
    }
}

/// Builds the smdh using `cytryna` library.
pub fn build_smdh(config: &CTRConfig) {
    let smdh = Smdh::builder()
        .with_short_desc(&config.name).unwrap()
        .with_long_desc(&config.description).unwrap()
        .with_publisher(&config.author).unwrap()
        .with_icon((&config.icon).try_into().unwrap())
        .build().expect("SMDH building failed");

    std::fs::write(config.path_smdh(), smdh.as_bytes()).expect("Failed to write SMDH data");
}

/// Builds the 3dsx using `3dsxtool`.
/// This will fail if `3dsxtool` is not within the running directory or in a directory found in $PATH
pub fn build_3dsx(config: &CTRConfig, verbose: bool) {
    let mut command = Command::new("3dsxtool");
    command
        .arg(&config.target_path)
        .arg(config.path_3dsx())
        .arg(format!("--smdh={}", config.path_smdh().to_string_lossy()));

    // If romfs directory exists, automatically include it
    let (romfs_path, is_default_romfs) = get_romfs_path(config);
    if romfs_path.is_dir() {
        eprintln!("Adding RomFS from {}", romfs_path.display());
        command.arg(format!("--romfs={}", romfs_path.to_string_lossy()));
    } else if !is_default_romfs {
        eprintln!(
            "Could not find configured RomFS dir: {}",
            romfs_path.display()
        );
        process::exit(1);
    }

    if verbose {
        print_command(&command);
    }

    let mut process = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("3dsxtool command failed, most likely due to '3dsxtool' not being in $PATH");

    let status = process.wait().unwrap();

    if !status.success() {
        process::exit(status.code().unwrap_or(1));
    }
}

/// Link the generated 3dsx to a 3ds to execute and test using `3dslink`.
/// This will fail if `3dslink` is not within the running directory or in a directory found in $PATH
pub fn link(config: &CTRConfig, run_args: &Run, verbose: bool) {
    let mut command = Command::new("3dslink");
    command
        .arg(config.path_3dsx())
        .args(run_args.get_3dslink_args())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if verbose {
        print_command(&command);
    }

    let status = command.spawn().unwrap().wait().unwrap();

    if !status.success() {
        process::exit(status.code().unwrap_or(1));
    }
}

/// Read the `RomFS` path from the Cargo manifest. If it's unset, use the default.
/// The returned boolean is true when the default is used.
pub fn get_romfs_path(config: &CTRConfig) -> (PathBuf, bool) {
    let manifest_path = &config.cargo_manifest_path;
    let manifest_str = std::fs::read_to_string(manifest_path)
        .unwrap_or_else(|e| panic!("Could not open {}: {e}", manifest_path.display()));
    let manifest_data: toml::Value =
        toml::de::from_str(&manifest_str).expect("Could not parse Cargo manifest as TOML");

    // Find the romfs setting and compute the path
    let mut is_default = false;
    let romfs_dir_setting = manifest_data
        .as_table()
        .and_then(|table| table.get("package"))
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("metadata"))
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("cargo-3ds"))
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("romfs_dir"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| {
            is_default = true;
            "romfs"
        });
    let mut romfs_path = manifest_path.clone();
    romfs_path.pop(); // Pop Cargo.toml
    romfs_path.push(romfs_dir_setting);

    (romfs_path, is_default)
}

#[derive(Default)]
pub struct CTRConfig {
    name: String,
    author: String,
    description: String,
    icon: image::DynamicImage,
    target_path: PathBuf,
    cargo_manifest_path: PathBuf,
}

impl CTRConfig {
    pub fn path_3dsx(&self) -> PathBuf {
        self.target_path.with_extension("3dsx")
    }

    pub fn path_smdh(&self) -> PathBuf {
        self.target_path.with_extension("smdh")
    }
}

#[derive(Ord, PartialOrd, PartialEq, Eq, Debug)]
pub struct CommitDate {
    year: i32,
    month: i32,
    day: i32,
}

impl CommitDate {
    fn parse(date: &str) -> Option<Self> {
        let mut iter = date.split('-');

        let year = iter.next()?.parse().ok()?;
        let month = iter.next()?.parse().ok()?;
        let day = iter.next()?.parse().ok()?;

        Some(Self { year, month, day })
    }
}

impl fmt::Display for CommitDate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

const MINIMUM_COMMIT_DATE: CommitDate = CommitDate {
    year: 2023,
    month: 5,
    day: 31,
};
const MINIMUM_RUSTC_VERSION: Version = Version::new(1, 70, 0);
