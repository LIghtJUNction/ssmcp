//! Decisions entered by the operator in the device CLI.
use super::controller_accounts;
use crate::Result;
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
};

const HELP: &str = "Usage: ssmcp controllers [list|history] [--json]\n       ssmcp controllers approve|revoke RELAY PROVIDER SUBJECT\n\nList the controller accounts observed by this device. Choose an exact entry to approve or revoke in your local terminal.";

pub(super) fn run(args: &[OsString]) -> Result<()> {
    let words = args
        .iter()
        .map(|s| s.to_str().ok_or("expected UTF-8 arguments"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    match words.as_slice() {
        ["--help" | "-h"] => println!("{HELP}"),
        [] | ["list"] | ["--json"] | ["list", "--json"] => {
            let entries = controller_accounts::history()?;
            if words.last() == Some(&"--json") {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("This device has no recorded controller accounts.");
            } else {
                for e in entries {
                    println!(
                        "{}  {}  {}  {}  {:?}  last seen {}",
                        e.relay,
                        e.provider,
                        e.subject,
                        e.display_name(),
                        e.status,
                        e.last_seen
                    );
                }
            }
        }
        ["history"] | ["history", "--json"] => {
            let events = controller_accounts::events()?;
            if words.last() == Some(&"--json") {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else if events.is_empty() {
                println!("No controller events recorded.");
            } else {
                for event in events {
                    println!(
                        "{}  {}  {:?}  {}  {} #{}  {}  {}",
                        event.sequence,
                        event.at,
                        event.kind,
                        event.relay,
                        event.provider,
                        event.subject,
                        event.label,
                        event.connection_id.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        [action @ ("approve" | "revoke"), relay, provider, subject] => {
            if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                return Err(
                    "confirm this change interactively in the controlled device's terminal".into(),
                );
            }
            let origin = crate::server_endpoint::normalize_server(relay)?;
            let entry = controller_accounts::history()?.into_iter().find(|e| e.relay == origin && e.provider == *provider && e.subject == *subject)
                .ok_or("unknown controller identity; choose the exact entry shown by ssmcp controllers")?;
            let mut prompt = io::stderr().lock();
            writeln!(
                prompt,
                "{} {} ({} #{}) on {}?",
                action,
                entry.display_name(),
                entry.provider,
                entry.subject,
                entry.relay
            )?;
            write!(prompt, "Type yes to confirm: ")?;
            prompt.flush()?;
            let mut reply = String::new();
            io::stdin().read_line(&mut reply)?;
            if !reply.trim().eq_ignore_ascii_case("yes") {
                return Err("decision cancelled".into());
            }
            let entry =
                controller_accounts::decide(&origin, provider, subject, *action == "approve")?;
            writeln!(
                prompt,
                "{} ({} #{}) on {}: {:?}",
                entry.display_name(),
                entry.provider,
                entry.subject,
                entry.relay,
                entry.status
            )?;
        }
        _ => return Err(HELP.into()),
    }
    Ok(())
}
