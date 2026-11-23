//! Isolated host prototype. Admission capabilities are supplied by the fixture.
//! Each host fixes the compiler, target defaults, environment and artifact format.
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, process::Stdio};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::oneshot,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Build {
    pub grant: u64,
    pub snapshot: &'static str,
}

pub type Receipt = [u8; 32];
type Result = std::result::Result<Receipt, String>;

struct Running {
    child: Child,
    output: PathBuf,
    waiters: Vec<oneshot::Sender<Result>>,
}

pub struct Host {
    pub compiler: PathBuf,
    pub root: PathBuf,
    pub capacity: usize,
    pub launches: usize,
    pub avoided: usize,
    active: HashMap<Build, Running>,
}

impl Host {
    pub fn new(compiler: PathBuf, root: PathBuf, capacity: usize) -> Self {
        Self {
            compiler,
            root,
            capacity,
            launches: 0,
            avoided: 0,
            active: HashMap::new(),
        }
    }

    pub fn submit(&mut self, build: Build) -> std::io::Result<oneshot::Receiver<Result>> {
        let (send, receive) = oneshot::channel();
        if let Some(job) = self.active.get_mut(&build) {
            job.waiters.retain(|sender| !sender.is_closed());
            if job.waiters.len() >= self.capacity {
                return Err(std::io::Error::other("too many build waiters"));
            }
            job.waiters.push(send);
            return Ok(receive);
        }
        if self.active.len() >= self.capacity {
            return Err(std::io::Error::other("too many builds"));
        }
        let dir = self.root.join(self.launches.to_string());
        std::fs::create_dir(&dir)?;
        let output = dir.join("build.o");
        let child = Command::new(&self.compiler)
            .current_dir(&dir)
            .env_clear()
            .args([
                "--crate-name",
                "managed_fixture",
                "--crate-type",
                "lib",
                "--emit=obj",
                "-o",
            ])
            .arg(&output)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        self.launches += 1;
        self.active.insert(
            build,
            Running {
                child,
                output,
                waiters: vec![send],
            },
        );
        Ok(receive)
    }

    // Source is immutable and host-owned. The fixture calls finish after all
    // admissions so rustc's stdin is a deterministic in-flight barrier.
    pub async fn finish(&mut self, build: Build, accept: bool) {
        let mut job = self.active.remove(&build).expect("active build");
        if job.waiters.iter().all(oneshot::Sender::is_closed) {
            let _ = job.child.kill().await;
            return;
        }
        let result = compile(&mut job, build.snapshot)
            .await
            .map_err(|e| e.to_string());
        let success = result.is_ok() && accept;
        let mut delivered = 0usize;
        for waiter in job.waiters {
            let reply = if accept {
                result.clone()
            } else {
                Err("grant revoked".into())
            };
            if waiter.send(reply).is_ok() && success {
                delivered += 1;
            }
        }
        self.avoided += delivered.saturating_sub(1);
    }
}

async fn compile(job: &mut Running, snapshot: &str) -> std::io::Result<Receipt> {
    let mut stdin = job.child.stdin.take().expect("compiler stdin");
    stdin.write_all(snapshot.as_bytes()).await?;
    drop(stdin);
    if !job.child.wait().await?.success() {
        return Err(std::io::Error::other("compiler failed"));
    }
    let artifact = std::fs::read(&job.output)?;
    if artifact.is_empty() {
        return Err(std::io::Error::other("empty artifact"));
    }
    Ok(Sha256::digest(artifact).into())
}
