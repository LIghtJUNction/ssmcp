//! Only the module manager reads this file. The HTTP server serves the public app.
use crate::Result;
use std::path::Path;

const TEMPLATE: &str = include_str!("../../../packaging/magisk/src/ssmcp/webroot/index.html");
const HOME: &str = "http://127.0.0.1:42387/";

pub(super) fn publish(host: &str, token: &str) -> Result<()> {
    let executable = std::env::current_exe()?;
    let Some(bin) = executable.parent().filter(|path| path.ends_with("bin")) else {
        return Ok(());
    };
    if let Some(module) = bin
        .parent()
        .filter(|path| path.join("module.prop").is_file())
    {
        write_entry(module, host, token)?;
    }
    Ok(())
}

fn write_entry(module: &Path, host: &str, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let webroot = module.join("webroot");
    if !std::fs::symlink_metadata(&webroot)?.is_dir() {
        return Err("invalid module webroot".into());
    }
    let page = TEMPLATE.replace(HOME, &format!("http://{host}/#{token}"));
    // A fresh root-only file avoids ever placing the credential in a public
    // template, process argument or partly written WebUI entry.
    let staged = module.join(format!(".webui-{}", super::pairing_token()?));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let result = file
        .write_all(page.as_bytes())
        .and_then(|()| std::fs::rename(&staged, webroot.join("index.html")));
    if result.is_err() {
        let _ = std::fs::remove_file(staged);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rotates_the_manager_link_without_publishing_a_credential() {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-module-ui-{}",
            super::super::pairing_token().unwrap()
        ));
        let webroot = root.join("webroot");
        std::fs::create_dir_all(&webroot).unwrap();
        for token in ["a".repeat(64), "b".repeat(64)] {
            write_entry(&root, "127.0.0.1:42387", &token).unwrap();
            let path = webroot.join("index.html");
            let page = std::fs::read_to_string(&path).unwrap();
            assert_eq!(page.matches(&format!("{HOME}#{token}")).count(), 2);
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(std::fs::read_dir(&webroot).unwrap().count(), 1);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
