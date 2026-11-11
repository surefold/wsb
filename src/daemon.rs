//! A compile server that keeps the Kotlin compiler loaded between compiles.
//!
//! Starting `kotlinc` costs a new JVM and a compiler that has not warmed up
//! yet, about a second before any work and slower work after it. The server
//! (`daemon/Server.java`) loads the compiler once, in a JVM that stays up,
//! and compiles each request in a thread of its own. `wsb` starts it the
//! first time it needs it, and it exits after [`IDLE`] with nothing to do.
//!
//! It listens on a Unix socket in `$TMPDIR/wsb-<uid>/`, a folder only its
//! user can open. One server runs for each Kotlin installation and Java.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::abi::{Fnv, hash};

/// The server's source, run with Java's source launcher.
const SERVER: &str = include_str!("daemon/Server.java");

/// How long the server waits with nothing to do before it exits.
pub const IDLE: Duration = Duration::from_secs(3 * 60 * 60);

/// How long to wait for a new server to start answering.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// A Kotlin installation: the folder holding `kotlin-compiler.jar`.
#[derive(Debug, Clone)]
pub struct KotlinHome {
    pub lib: PathBuf,
}

impl KotlinHome {
    /// The installation `kotlinc` on the `PATH` belongs to.
    pub fn find() -> Option<KotlinHome> {
        let kotlinc = find_on_path("kotlinc")?;
        let real = fs::canonicalize(kotlinc).ok()?;
        let bin = real.parent()?;
        // Homebrew's `kotlinc` is a script that runs the one in `libexec`.
        [bin.join("../lib"), bin.join("../libexec/lib")]
            .into_iter()
            .find(|lib| lib.join("kotlin-compiler.jar").is_file())
            .and_then(|lib| fs::canonicalize(lib).ok())
            .map(|lib| KotlinHome { lib })
    }

    /// The version in the installation's `build.txt`, read without
    /// starting a JVM.
    pub fn version(&self) -> Option<String> {
        let text = fs::read_to_string(self.lib.join("../build.txt")).ok()?;
        let version = text.trim().split('-').next()?.to_string();
        (!version.is_empty()).then_some(version)
    }
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// What a compile through the server printed, and whether it succeeded.
pub struct Compiled {
    pub ok: bool,
    /// The server failed, rather than the code: compile another way.
    pub crashed: bool,
    pub printed: String,
}

/// A connection to the compile server for one Kotlin installation.
pub struct Daemon {
    home: KotlinHome,
    java: PathBuf,
    folder: PathBuf,
    socket: PathBuf,
    /// How long a server this starts waits with nothing to do.
    idle: Duration,
}

impl Daemon {
    /// Finds where the server for this installation listens. It is started
    /// only when a request needs it.
    #[cfg(unix)]
    pub fn new(home: KotlinHome) -> io::Result<Daemon> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let java = match std::env::var_os("JAVA_HOME") {
            Some(h) => PathBuf::from(h).join("bin/java"),
            None => {
                find_on_path("java").ok_or_else(|| io::Error::other("no `java` on the PATH"))?
            }
        };
        let me = fs::metadata(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))?.uid();
        let base = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let folder = base.join(format!("wsb-{me}"));
        fs::create_dir_all(&folder)?;
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
        let meta = fs::metadata(&folder)?;
        if meta.uid() != me || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "{} is not private to this user",
                folder.display()
            )));
        }
        let mut hash = Fnv::new();
        hash.update(home.lib.as_os_str().as_encoded_bytes());
        hash.update(java.as_os_str().as_encoded_bytes());
        hash.update(SERVER.as_bytes());
        let socket = folder.join(format!("{}.sock", &hash.finish()[..16]));
        // Unix socket paths are limited to about 100 bytes.
        if socket.as_os_str().len() > 100 {
            return Err(io::Error::other("the socket path would be too long"));
        }
        Ok(Daemon {
            home,
            java,
            folder,
            socket,
            idle: IDLE,
        })
    }

    /// Starts any server with this idle time instead of [`IDLE`]. A server
    /// already running keeps its own.
    pub fn with_idle(mut self, idle: Duration) -> Daemon {
        self.idle = idle;
        self
    }

    #[cfg(not(unix))]
    pub fn new(_home: KotlinHome) -> io::Result<Daemon> {
        Err(io::Error::other("the compile server needs Unix sockets"))
    }

    /// Runs `kotlinc` with these arguments in the server, starting it if
    /// it is not running.
    pub fn compile(&self, args: &[OsString]) -> io::Result<Compiled> {
        let mut words: Vec<&OsStr> = vec![OsStr::new("compile")];
        words.extend(args.iter().map(OsString::as_os_str));
        let answer = self.request(&words, true)?;
        let (status, printed) = answer.split_once('\n').unwrap_or((answer.as_str(), ""));
        Ok(Compiled {
            ok: status == "OK",
            crashed: status == "CRASHED",
            printed: printed.trim().to_string(),
        })
    }

    /// Asks a running server to exit. Returns whether one was running.
    pub fn stop(&self) -> io::Result<bool> {
        match self.request(&[OsStr::new("stop")], false) {
            Ok(_) => Ok(true),
            Err(e) if not_running(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    #[cfg(unix)]
    fn request(&self, words: &[&OsStr], start: bool) -> io::Result<String> {
        use std::os::unix::net::UnixStream;
        let mut stream = match UnixStream::connect(&self.socket) {
            Ok(s) => s,
            Err(e) if start && not_running(&e) => self.start()?,
            Err(e) => return Err(e),
        };
        let mut message = Vec::new();
        for word in words {
            let bytes = word.as_encoded_bytes();
            if bytes.contains(&0) {
                return Err(io::Error::other("an argument holds a zero byte"));
            }
            message.extend_from_slice(bytes);
            message.push(0);
        }
        // The length first: the server does not wait for this side to close.
        stream.write_all(format!("{}\n", message.len()).as_bytes())?;
        stream.write_all(&message)?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer)?;
        if answer.is_empty() {
            return Err(io::Error::other(
                "the compile server closed without answering",
            ));
        }
        Ok(answer)
    }

    #[cfg(not(unix))]
    fn request(&self, _words: &[&OsStr], _start: bool) -> io::Result<String> {
        Err(io::Error::other("the compile server needs Unix sockets"))
    }

    /// Starts the server and waits until it answers.
    #[cfg(unix)]
    fn start(&self) -> io::Result<std::os::unix::net::UnixStream> {
        use std::os::unix::process::CommandExt;
        let source = self
            .folder
            .join(format!("Server-{}.java", &hash(SERVER.as_bytes())[..16]));
        if !source.is_file() {
            let temporary = source.with_extension(format!("tmp-{}", std::process::id()));
            fs::write(&temporary, SERVER)?;
            fs::rename(&temporary, &source)?;
        }
        let log = fs::File::create(self.socket.with_extension("log"))?;
        Command::new(&self.java)
            .args(["-Xmx1g", "-Xss16m", "-Djava.awt.headless=true", "-cp"])
            .arg(self.home.lib.join("kotlin-compiler.jar"))
            .arg(&source)
            .arg(&self.socket)
            .arg(self.idle.as_secs().max(1).to_string())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Not in the terminal's process group, so Ctrl-C on `wsb` leaves it.
            .process_group(0)
            .spawn()?;
        let started = Instant::now();
        loop {
            match std::os::unix::net::UnixStream::connect(&self.socket) {
                Ok(s) => return Ok(s),
                Err(e) if not_running(&e) && started.elapsed() < START_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "the compile server did not start ({e}); see {}",
                            self.socket.with_extension("log").display()
                        ),
                    ));
                }
            }
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

fn not_running(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}
