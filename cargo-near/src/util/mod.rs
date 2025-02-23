use crate::cargo::manifest::CargoManifestPath;
use crate::ColorPreference;
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{Artifact, Message};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::{env, thread};

mod print;
pub(crate) use print::*;

pub(crate) const fn dylib_extension() -> &'static str {
    #[cfg(target_os = "linux")]
    return "so";

    #[cfg(target_os = "macos")]
    return "dylib";

    #[cfg(target_os = "windows")]
    return "dll";

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    compile_error!("Unsupported platform");
}

/// Invokes `cargo` with the subcommand `command`, the supplied `args` and set `env` variables.
///
/// If `working_dir` is set, cargo process will be spawned in the specified directory.
///
/// Returns execution standard output as a byte array.
fn invoke_cargo<A, P, E, S, EK, EV>(
    command: &str,
    args: A,
    working_dir: Option<P>,
    env: E,
    color: ColorPreference,
) -> Result<Vec<Artifact>>
where
    A: IntoIterator<Item = S>,
    P: AsRef<Utf8Path>,
    E: IntoIterator<Item = (EK, EV)>,
    S: AsRef<OsStr>,
    EK: AsRef<OsStr>,
    EV: AsRef<OsStr>,
{
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(cargo);

    cmd.envs(env);

    if let Some(path) = working_dir {
        let path = path.as_ref();
        log::debug!("Setting cargo working dir to '{}'", path);
        cmd.current_dir(path);
    }

    cmd.arg(command);
    cmd.args(args);

    match color {
        ColorPreference::Always => cmd.args(["--color", "always"]),
        ColorPreference::Never => cmd.args(["--color", "never"]),
    };

    log::info!("Invoking cargo: {:?}", cmd);

    let mut child = cmd
        // capture the stdout to return from this function as bytes
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(format!("Error executing `{:?}`", cmd))?;
    let child_stdout = child
        .stdout
        .take()
        .context("could not attach to child stdout")?;
    let child_stderr = child
        .stderr
        .take()
        .context("could not attach to child stderr")?;

    // stdout and stderr have to be processed concurrently to not block the process from progressing
    let thread_stdout = thread::spawn(move || -> Result<_, std::io::Error> {
        let mut artifacts = vec![];
        let stdout_reader = std::io::BufReader::new(child_stdout);
        for message in Message::parse_stream(stdout_reader) {
            match message? {
                Message::CompilerArtifact(artifact) => {
                    artifacts.push(artifact);
                }
                Message::CompilerMessage(message) => {
                    if let Some(msg) = message.message.rendered {
                        for line in msg.lines() {
                            eprintln!(" │ {}", line);
                        }
                    }
                }
                _ => {}
            };
        }

        Ok(artifacts)
    });
    let thread_stderr = thread::spawn(move || {
        let stderr_reader = BufReader::new(child_stderr);
        let stderr_lines = stderr_reader.lines();
        for line in stderr_lines {
            eprintln!(" │ {}", line.expect("failed to read cargo stderr"));
        }
    });

    let result = thread_stdout.join().expect("failed to join stdout thread");
    thread_stderr.join().expect("failed to join stderr thread");

    let output = child.wait()?;

    if output.success() {
        Ok(result?)
    } else {
        anyhow::bail!("`{:?}` failed with exit code: {:?}", cmd, output.code());
    }
}

pub(crate) fn invoke_rustup<I, S>(args: I) -> anyhow::Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let rustup = env::var("RUSTUP").unwrap_or_else(|_| "rustup".to_string());

    let mut cmd = Command::new(rustup);
    cmd.args(args);

    log::info!("Invoking rustup: {:?}", cmd);

    let child = cmd
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context(format!("Error executing `{:?}`", cmd))?;

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        anyhow::bail!(
            "`{:?}` failed with exit code: {:?}",
            cmd,
            output.status.code()
        );
    }
}

pub struct CompilationArtifact {
    pub path: Utf8PathBuf,
    pub fresh: bool,
}

/// Builds the cargo project with manifest located at `manifest_path` and returns the path to the generated artifact.
pub(crate) fn compile_project(
    manifest_path: &CargoManifestPath,
    args: &[&str],
    mut env: Vec<(&str, &str)>,
    artifact_extension: &str,
    hide_warnings: bool,
    color: ColorPreference,
) -> anyhow::Result<CompilationArtifact> {
    let mut final_env = BTreeMap::new();

    if hide_warnings {
        env.push(("RUSTFLAGS", "-Awarnings"));
    }

    for (key, value) in env {
        match key {
            "RUSTFLAGS" => {
                let rustflags: &mut String = final_env
                    .entry(key)
                    .or_insert_with(|| std::env::var(key).unwrap_or_default());
                if !rustflags.is_empty() {
                    rustflags.push(' ');
                }
                rustflags.push_str(value);
            }
            _ => {
                final_env.insert(key, value.to_string());
            }
        }
    }

    let artifacts = invoke_cargo(
        "build",
        [&["--message-format=json-render-diagnostics"], args].concat(),
        manifest_path.directory().ok(),
        final_env.iter(),
        color,
    )?;

    // We find the last compiler artifact message which should contain information about the
    // resulting dylib file
    let compile_artifact = artifacts.last().ok_or_else(|| {
        anyhow::anyhow!(
            "Cargo failed to produce any compilation artifacts. \
                 Please check that your project contains a NEAR smart contract."
        )
    })?;
    // The project could have generated many auxiliary files, we are only interested in
    // dylib files with a specific (platform-dependent) extension
    let dylib_files = compile_artifact
        .filenames
        .iter()
        .cloned()
        .filter(|f| {
            f.extension()
                .map(|e| e == artifact_extension)
                .unwrap_or(false)
        })
        .collect();
    let mut dylib_files_iter = Vec::into_iter(dylib_files);
    match (dylib_files_iter.next(), dylib_files_iter.next()) {
        (None, None) => Err(anyhow::anyhow!(
            "Compilation resulted in no '.{}' target files. \
                 Please check that your project contains a NEAR smart contract.",
            artifact_extension
        )),
        (Some(path), None) => Ok(CompilationArtifact {
            path,
            fresh: !compile_artifact.fresh,
        }),
        _ => Err(anyhow::anyhow!(
            "Compilation resulted in more than one '.{}' target file: {:?}",
            artifact_extension,
            dylib_files_iter.as_slice()
        )),
    }
}

/// Create the directory if it doesn't exist, and return the absolute path to it.
pub(crate) fn force_canonicalize_dir(dir: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    fs::create_dir_all(&dir).with_context(|| format!("failed to create directory `{}`", dir))?;
    dir.canonicalize_utf8()
        .with_context(|| format!("failed to access output directory `{}`", dir))
}

/// Copy a file to a destination.
///
/// Does nothing if the destination is the same as the source to avoid truncating the file.
pub(crate) fn copy(from: &Utf8Path, to: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
    let out_path = to.join(from.file_name().unwrap());
    if from != out_path {
        fs::copy(&from, &out_path)
            .with_context(|| format!("failed to copy `{}` to `{}`", from, out_path))?;
    }
    Ok(out_path)
}

pub(crate) fn extract_abi_entries(
    dylib_path: &Utf8Path,
) -> anyhow::Result<Vec<near_abi::__private::ChunkedAbiEntry>> {
    let dylib_file_contents = fs::read(&dylib_path)?;
    let object = symbolic_debuginfo::Object::parse(&dylib_file_contents)?;
    log::debug!(
        "A dylib was built at {:?} with format {} for architecture {}",
        &dylib_path,
        &object.file_format(),
        &object.arch()
    );
    let near_abi_symbols = object
        .symbols()
        .flat_map(|sym| sym.name)
        .filter(|sym_name| sym_name.starts_with("__near_abi_"))
        .collect::<HashSet<_>>();
    if near_abi_symbols.is_empty() {
        anyhow::bail!("No NEAR ABI symbols found in the dylib");
    }
    log::debug!("Detected NEAR ABI symbols: {:?}", &near_abi_symbols);

    let mut entries = vec![];
    unsafe {
        let lib = libloading::Library::new(dylib_path)?;
        for symbol in near_abi_symbols {
            let entry: libloading::Symbol<extern "C" fn() -> (*const u8, usize)> =
                lib.get(symbol.as_bytes())?;
            let (ptr, len) = entry();
            let data = Vec::from_raw_parts(ptr as *mut _, len, len);
            match serde_json::from_slice(&data) {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    // unfortunately, we're unable to extract the raw error without Display-ing it first
                    let mut err_str = err.to_string();
                    if let Some((msg, rest)) = err_str.rsplit_once(" at line ") {
                        if let Some((line, col)) = rest.rsplit_once(" column ") {
                            if line.chars().all(|c| c.is_numeric())
                                && col.chars().all(|c| c.is_numeric())
                            {
                                err_str.truncate(msg.len());
                                err_str.shrink_to_fit();
                                anyhow::bail!(err_str);
                            }
                        }
                    }
                    anyhow::bail!(err);
                }
            };
        }
    }
    Ok(entries)
}
