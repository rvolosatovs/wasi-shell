// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(target_os = "wasi", feature(wasi_ext))]

use std::env::args_os;
use std::fmt::Display;
use std::fs::{self, File, ReadDir};
use std::io::{copy, stdin, stdout, Read, Write};
use std::net::TcpListener;
use std::ops::Deref;
#[cfg(unix)]
use std::os::unix::io::OwnedFd;
#[cfg(target_os = "wasi")]
use std::os::wasi::io::OwnedFd;
use std::process;

use anyhow::{anyhow, bail, Context};
use camino::{Utf8Path, Utf8PathBuf};

struct WorkingDir {
    path: Utf8PathBuf,
}

impl Deref for WorkingDir {
    type Target = Utf8PathBuf;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Display for WorkingDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}

impl WorkingDir {
    fn open(path: impl Into<Utf8PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        #[cfg(unix)]
        let path = path
            .canonicalize_utf8()
            .context("failed to canonicalize path")?;
        // TODO: Hold the returned FD?
        _ = File::open(&path).with_context(|| format!("failed to open `{path}`"))?;
        Ok(Self { path })
    }
}

#[inline]
fn strip_surround<const C: char>(s: &str) -> Option<&str> {
    s.strip_prefix(C).and_then(|s| s.strip_suffix(C))
}

/// Recursively remove surrounding double and single quote character pairs and trailing whitespace
fn unquote(s: &str) -> &str {
    let s = s.trim();
    let s = strip_surround::<'"'>(s).map(unquote).unwrap_or(s);
    let s = strip_surround::<'\''>(s).map(unquote).unwrap_or(s);
    s
}

const COMMANDS: [&str; 8] = ["accept", "cat", "cd", "help", "echo", "exit", "ls", "pwd"];

const ACCEPT_USAGE: &str = "Usage: accept FILE";

#[inline]
fn into_listener(fd: impl Into<OwnedFd>) -> TcpListener {
    fd.into().into()
}

fn accept(dir: &WorkingDir, path: impl AsRef<str>) -> anyhow::Result<Vec<u8>> {
    let path = dir.join(unquote(path.as_ref()));
    let (mut stream, _) = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .map(into_listener)
        .with_context(|| format!("failed to open `{path}`"))?
        .accept()
        .with_context(|| format!("failed to accept connection on `{path}`"))?;

    let mut buf = Default::default();
    stream
        .read_to_end(&mut buf)
        .context("failed to read from stream")?;
    Ok(buf)
}

const CAT_USAGE: &str = "Usage: cat FILE";

fn cat(dir: &WorkingDir, path: impl AsRef<str>) -> anyhow::Result<Vec<u8>> {
    let path = dir.join(unquote(path.as_ref()));
    fs::read(&path).with_context(|| format!("failed to read `{path}`"))
}

fn cd(dir: &WorkingDir, path: impl AsRef<str>) -> anyhow::Result<WorkingDir> {
    let path = Utf8Path::new(unquote(path.as_ref()));
    if path.is_absolute() {
        WorkingDir::open(path)
    } else {
        WorkingDir::open(dir.join(path))
    }
}

const ECHO_USAGE: &str = "Usage: echo [WORD|\"TEXT\"|'TEXT'] > FILE";

fn echo(dir: &WorkingDir, args: impl AsRef<str>) -> anyhow::Result<()> {
    let (text, path) = args.as_ref().rsplit_once('>').context("missing `>`")?;
    let text = unquote(text);
    let path = dir.join(unquote(path));
    fs::write(&path, text).with_context(|| format!("failed to write `{text}` to `{path}`"))
}

const EXIT_USAGE: &str = "Usage: exit";

fn exit() -> Effect {
    Effect {
        exit: Some(0),
        ..Default::default()
    }
}

const HELP_USAGE: &str = "Usage: help";

fn help() -> Vec<u8> {
    format!(r#"Available commands: {}"#, COMMANDS.join(r#", "#)).into()
}

fn ls(dir: &WorkingDir, path: Option<&str>) -> anyhow::Result<Vec<u8>> {
    #[inline]
    fn format_dir(dir: ReadDir) -> anyhow::Result<Vec<u8>> {
        dir.map(|entry| {
            entry
                .context("failed to read directory entry")?
                .file_name()
                .into_string()
                .map_err(|name| anyhow!("failed to parse entry name `{}`", name.to_string_lossy()))
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|names| names.join(" ").into())
    }

    if let Some(path) = path {
        let path = dir.join(unquote(path));
        fs::read_dir(&path)
            .with_context(|| format!("failed to list directory `{path}`"))
            .and_then(format_dir)
    } else {
        fs::read_dir(dir.deref())
            .with_context(|| format!("failed to list working directory contents in `{dir}`"))
            .and_then(format_dir)
    }
}

const PWD_USAGE: &str = "Usage: pwd";

fn pwd(dir: &WorkingDir) -> Vec<u8> {
    format!("{dir}").into()
}

/// Effect of execution of a command
#[derive(Default)]
struct Effect {
    /// New working directory
    dir: Option<WorkingDir>,

    /// Standard output
    out: Option<Vec<u8>>,

    /// Whether the shell should exit
    exit: Option<i32>,
}

impl From<()> for Effect {
    fn from((): ()) -> Self {
        Default::default()
    }
}

impl From<Option<WorkingDir>> for Effect {
    fn from(dir: Option<WorkingDir>) -> Self {
        Self {
            dir,
            ..Default::default()
        }
    }
}

impl From<WorkingDir> for Effect {
    fn from(dir: WorkingDir) -> Self {
        Some(dir).into()
    }
}

impl From<Option<Vec<u8>>> for Effect {
    fn from(out: Option<Vec<u8>>) -> Self {
        Self {
            out,
            ..Default::default()
        }
    }
}

impl From<Vec<u8>> for Effect {
    fn from(out: Vec<u8>) -> Self {
        Some(out).into()
    }
}

fn handle(dir: &WorkingDir, line: impl AsRef<str>) -> anyhow::Result<Effect> {
    let line = line.as_ref().trim();
    if line.is_empty() {
        return Ok(Default::default());
    } else if !line
        .chars()
        .next()
        .map(char::is_alphanumeric)
        .unwrap_or_default()
    {
        bail!("line must start with an alphanumeric character or whitespace")
    }
    match line.split_once(' ') {
        None if line == "accept" => bail!(ACCEPT_USAGE),
        Some(("accept", path)) => accept(dir, path).map(Into::into),

        None if line == "cat" => bail!(CAT_USAGE),
        Some(("cat", args)) => cat(dir, args).map(Into::into),

        None if line == "cd" => Ok(Default::default()),
        Some(("cd", args)) => cd(dir, args).map(Into::into),

        None if line == "echo" => bail!(ECHO_USAGE),
        Some(("echo", args)) => echo(dir, args).map(Into::into),

        None if line == "exit" => Ok(exit()),
        Some(("exit", _)) => bail!(EXIT_USAGE),

        None if line == "help" => Ok(help().into()),
        Some(("help", _)) => bail!(HELP_USAGE),

        None if line == "ls" => ls(dir, None).map(Into::into),
        Some(("ls", path)) => ls(dir, Some(path)).map(Into::into),

        None if line == "pwd" => Ok(pwd(dir).into()),
        Some(("pwd", _)) => bail!(PWD_USAGE),

        _ => bail!("failed to parse line"),
    }
}

fn prompt(dir: &WorkingDir) -> String {
    format!("{dir} $ ")
}

fn main() -> anyhow::Result<()> {
    if args_os().count() != 1 {
        bail!("wash takes exactly one argument: the executable path")
    }
    let mut stdout = stdout();
    let dir = WorkingDir::open("/")?;
    eprint!("{}", prompt(&dir));
    stdin()
        .lines()
        .try_fold(dir, |dir, line| {
            let line = line.context("failed to read line from STDIN")?;
            let dir = match handle(&dir, line).and_then(|Effect { dir, out, exit }| {
                if let Some(out) = out {
                    copy(&mut out.as_slice(), &mut stdout)
                        .context("failed to write output to STDOUT")?;
                    stdout
                        .write(b"\n")
                        .context("failed to write newline to STDOUT")?;
                }
                if let Some(code) = exit {
                    process::exit(code)
                }
                Ok(dir)
            }) {
                Ok(None) => dir,
                Ok(Some(dir)) => dir,
                Err(e) => {
                    eprintln!("Error: {:?}", e);
                    dir
                }
            };
            eprint!("{}", prompt(&dir));
            Ok(dir)
        })
        .map(|_| ())
}
