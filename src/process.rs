//! Supervised helper processes with deadlines, output limits and process-group cleanup.

use super::*;

pub(crate) struct LimitedCommand {
    command: Command,
    deadline: Duration,
}

impl LimitedCommand {
    pub(crate) fn arg(&mut self, argument: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.command.arg(argument);
        self
    }

    pub(crate) fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.command.args(arguments);
        self
    }

    pub(crate) fn stdin(&mut self, stream: Stdio) -> &mut Self {
        self.command.stdin(stream);
        self
    }
    pub(crate) fn stdout(&mut self, stream: Stdio) -> &mut Self {
        self.command.stdout(stream);
        self
    }
    pub(crate) fn stderr(&mut self, stream: Stdio) -> &mut Self {
        self.command.stderr(stream);
        self
    }

    pub(crate) fn spawn(&mut self) -> std::io::Result<LimitedChild> {
        self.command.process_group(0);
        let mut child = self.command.spawn()?;
        let pid = child.id() as i32;
        Ok(LimitedChild {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            pid,
            deadline: std::time::Instant::now() + self.deadline,
        })
    }

    pub(crate) fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.command.stdout(Stdio::piped()).stderr(Stdio::piped());
        self.spawn()?.wait_with_output()
    }

    #[cfg(feature = "backend-portal")]
    pub(crate) fn status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.spawn()?.wait()
    }
}

impl std::ops::Deref for LimitedCommand {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.command
    }
}

impl std::ops::DerefMut for LimitedCommand {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}

pub(crate) struct LimitedChild {
    pub stdin: Option<std::process::ChildStdin>,
    pub stdout: Option<std::process::ChildStdout>,
    pub stderr: Option<std::process::ChildStderr>,
    pub(crate) child: std::process::Child,
    pub(crate) pid: i32,
    pub(crate) deadline: std::time::Instant,
}

impl LimitedChild {
    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                terminate_process_group(self.pid, Duration::from_millis(100));
                return Ok(status);
            }
            let remaining = self
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                terminate_process_group(self.pid, Duration::from_secs(1));
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "child process deadline exceeded",
                ));
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }

    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        signal_process_group(self.pid, 9);
        self.child.kill()
    }

    pub(crate) fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        let stdout = spawn_output_reader(self.stdout.take(), None);
        let stderr = spawn_output_reader(self.stderr.take(), None);
        let status = self.wait();
        let stdout = join_output_reader(stdout, self.pid)?;
        let stderr = join_output_reader(stderr, self.pid)?;
        Ok(std::process::Output {
            status: status?,
            stdout,
            stderr,
        })
    }
}

pub(crate) fn spawn_output_reader<R: Read + Send + 'static>(
    stream: Option<R>,
    limit: Option<usize>,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let Some(mut stream) = stream else {
            return Ok(Vec::new());
        };
        let mut bytes = Vec::new();
        match limit {
            Some(limit) => {
                stream
                    .by_ref()
                    .take(limit.saturating_add(1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > limit {
                    std::io::copy(&mut stream, &mut std::io::sink())?;
                }
            }
            None => {
                stream.read_to_end(&mut bytes)?;
            }
        }
        Ok(bytes)
    })
}

pub(crate) fn join_output_reader(
    reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    pid: i32,
) -> std::io::Result<Vec<u8>> {
    let cleanup_at = std::time::Instant::now() + Duration::from_millis(100);
    while !reader.is_finished() && std::time::Instant::now() < cleanup_at {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !reader.is_finished() {
        signal_process_group(pid, 15);
        std::thread::sleep(Duration::from_millis(50));
        signal_process_group(pid, 9);
    }
    reader
        .join()
        .map_err(|_| std::io::Error::other("child output reader panicked"))?
}

pub(crate) fn process_group_exists(pid: i32) -> bool {
    if unsafe { kill_process(-pid, 0) } == 0 {
        true
    } else {
        std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
    }
}

pub(crate) fn terminate_process_group(pid: i32, grace: Duration) {
    if !process_group_exists(pid) {
        return;
    }
    signal_process_group(pid, 15);
    let deadline = std::time::Instant::now() + grace;
    while process_group_exists(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    signal_process_group(pid, 9);
}

pub(crate) fn signal_process_group(pid: i32, signal: i32) {
    let _ = unsafe { kill_process(-pid, signal) };
}

#[link(name = "c")]
unsafe extern "C" {
    #[link_name = "kill"]
    pub(crate) fn kill_process(pid: i32, signal: i32) -> i32;
}

pub(crate) fn limited_command(duration: &str, program: &str) -> LimitedCommand {
    let seconds = duration
        .strip_suffix('s')
        .unwrap_or(duration)
        .parse::<u64>()
        .expect("limited_command duration must be seconds");
    let mut command = Command::new(program);
    command.process_group(0);
    LimitedCommand {
        command,
        deadline: Duration::from_secs(seconds),
    }
}
