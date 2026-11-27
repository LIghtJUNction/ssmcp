//! Session-local navigation shared by the terminal, browser and desktop bridge.
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use super::frontend::OwnedSession;
use serde_json::{Value, json};

pub(crate) struct Workbench {
    root: PathBuf,
    cwd: Mutex<PathBuf>,
    destination: Option<String>,
    children: tokio::sync::Mutex<Vec<OwnedSession>>,
}

impl Workbench {
    pub(crate) fn new(root: PathBuf, destination: Option<String>) -> crate::Result<Self> {
        let root = root.canonicalize()?;
        Ok(Self {
            cwd: Mutex::new(root.clone()),
            root,
            destination,
            children: tokio::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn cd(&self, path: &str) -> Result<PathBuf, String> {
        if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
            return Err("Enter an existing directory path (up to 4096 bytes)".into());
        }
        let mut cwd = self.cwd.lock().unwrap_or_else(|e| e.into_inner());
        let next = cwd
            .join(Path::new(path))
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !next.is_dir() || !next.starts_with(&self.root) {
            return Err("Choose a directory inside this session's workspace".into());
        }
        *cwd = next.clone();
        Ok(next)
    }

    pub(crate) fn can_create(&self) -> bool {
        self.destination.is_some()
    }

    pub(crate) async fn create(&self) -> crate::Result<Value> {
        let destination = self
            .destination
            .as_deref()
            .ok_or("New sessions require an explicit relay destination")?;
        let mut children = self
            .children
            .try_lock()
            .map_err(|_| "Another session is opening")?;
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        if children.len() >= 32 {
            return Err("This session has 32 live child sessions".into());
        }
        let cwd = self.cwd();
        let child = OwnedSession::launch(&std::env::current_exe()?, destination, &cwd).await?;
        let result = json!({"session":child.target(),"cwd":cwd});
        children.push(child);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cwd_is_scoped_relative_and_confined() {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-cd-{}",
            super::super::random_suffix().unwrap()
        ));
        std::fs::create_dir_all(root.join("one/two words")).unwrap();
        std::fs::write(root.join("file"), b"x").unwrap();
        let a = Workbench::new(root.clone(), None).unwrap();
        let b = Workbench::new(root.clone(), None).unwrap();
        a.cd("one").unwrap();
        let nested = a.cd("two words").unwrap();
        assert!(nested.ends_with("one/two words"));
        assert!(a.cd("..").unwrap().ends_with("one"));
        assert!(a.cd("../file").is_err());
        assert!(a.cd("../..").is_err());
        assert_eq!(a.cwd(), nested.parent().unwrap());
        assert_eq!(b.cwd(), root.canonicalize().unwrap());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.parent().unwrap(), root.join("escape")).unwrap();
            assert!(a.cd("../escape").is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
