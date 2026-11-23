use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt::Write as _,
};

const LOCK: &str = include_str!("../../Cargo.lock");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Request {
    Plain,
    Dependencies,
}

pub(super) fn request(args: &[OsString]) -> Result<Option<Request>, String> {
    let mut version = false;
    let mut all = false;
    for arg in args {
        match arg.to_str() {
            Some("--version" | "-V") if !version => version = true,
            Some("--all") if !all => all = true,
            _ if !version && !all => return Ok(None),
            _ => return Err("use --version alone or with --all".into()),
        }
    }
    match (version, all) {
        (true, false) => Ok(Some(Request::Plain)),
        (true, true) => Ok(Some(Request::Dependencies)),
        (false, true) => Err("--all requires --version".into()),
        (false, false) => Ok(None),
    }
}

#[derive(Deserialize)]
struct Lock {
    package: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

impl Package {
    fn key(&self) -> (&str, &str, Option<&str>) {
        (&self.name, &self.version, self.source.as_deref())
    }
}

struct Tree<'a> {
    packages: &'a [Package],
    by_name: BTreeMap<&'a str, Vec<usize>>,
    expanded: BTreeSet<usize>,
    ancestors: BTreeSet<usize>,
}

impl<'a> Tree<'a> {
    fn new(packages: &'a [Package]) -> Result<Self, String> {
        let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        let mut identities = BTreeSet::new();
        for (index, package) in packages.iter().enumerate() {
            if !identities.insert(package.key()) {
                return Err(format!(
                    "duplicate locked package identity: {:?}",
                    package.key()
                ));
            }
            by_name.entry(&package.name).or_default().push(index);
        }
        Ok(Self {
            packages,
            by_name,
            expanded: BTreeSet::new(),
            ancestors: BTreeSet::new(),
        })
    }

    fn resolve(&self, dependency: &str) -> Result<usize, String> {
        let mut parts = dependency.splitn(3, ' ');
        let name = parts.next().unwrap_or_default();
        let version = parts.next();
        let source = parts
            .next()
            .map(|source| {
                source
                    .strip_prefix('(')
                    .and_then(|source| source.strip_suffix(')'))
                    .ok_or_else(|| format!("invalid locked dependency source: {dependency}"))
            })
            .transpose()?;
        let mut matches = self
            .by_name
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&index| {
                let package = &self.packages[index];
                version.is_none_or(|version| package.version == version)
                    && source.is_none_or(|source| package.source.as_deref() == Some(source))
            });
        match (matches.next(), matches.next()) {
            (Some(index), None) => Ok(index),
            (None, _) => Err(format!("missing locked dependency: {dependency}")),
            _ => Err(format!("ambiguous locked dependency: {dependency}")),
        }
    }

    fn append(&mut self, out: &mut String, index: usize, prefix: &str) -> Result<(), String> {
        self.expanded.insert(index);
        self.ancestors.insert(index);
        let mut children = self.packages[index]
            .dependencies
            .iter()
            .map(|dependency| self.resolve(dependency))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort_unstable_by_key(|&index| self.packages[index].key());
        for (position, child) in children.iter().copied().enumerate() {
            let package = &self.packages[child];
            let last = position + 1 == children.len();
            let branch = if last { "└──" } else { "├──" };
            let (source, annotation) = (
                package.source.as_deref().unwrap_or("workspace/path"),
                if self.ancestors.contains(&child) {
                    " [cycle]"
                } else if self.expanded.contains(&child) {
                    " [already expanded]"
                } else {
                    ""
                },
            );
            writeln!(
                out,
                "{prefix}{branch} {} {} ({source}){annotation}",
                package.name, package.version
            )
            .expect("writing to String cannot fail");
            if annotation.is_empty() {
                let indent = if last { "    " } else { "│   " };
                self.append(out, child, &format!("{prefix}{indent}"))?;
            }
        }
        self.ancestors.remove(&index);
        Ok(())
    }
}

fn dependencies(lock: &str, version: &str) -> Result<String, String> {
    let lock: Lock =
        toml::from_str(lock).map_err(|error| format!("invalid embedded Cargo.lock: {error}"))?;
    let mut tree = Tree::new(&lock.package)?;
    let index = tree.resolve("ssmcp")?;
    let root = &lock.package[index];
    if root.version != version || root.source.is_some() {
        return Err("embedded Cargo.lock does not match the built ssmcp package".into());
    }
    let mut out = format!(
        "ssmcp {version}\nBuild-time Cargo.lock dependency tree (all locked targets, including build and dev dependencies)\n"
    );
    tree.append(&mut out, index, "")?;
    Ok(out)
}

pub(super) fn render(request: Request) -> Result<String, String> {
    let version = env!("CARGO_PKG_VERSION");
    match request {
        Request::Plain => Ok(format!("ssmcp {version}\n")),
        Request::Dependencies => dependencies(LOCK, version),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_cannot_hide_mixed_startup_options_or_consume_harness_tail() {
        use super::super::Options;

        for args in [
            ["--no-browser", "--version", "--all"],
            ["--no-browser", "--all", "--version"],
            ["--no-browser", "-V", "--all"],
        ] {
            let args = args.map(OsString::from);
            assert_eq!(request(&args).unwrap(), None);
            let error = Options::parse(args)
                .err()
                .expect("mixed version options accepted");
            assert!(error.contains("--version"));
        }
        let tail = ["--", "--all", "--version"];
        let args = [
            "example.test",
            "--load-harness",
            "codex",
            tail[0],
            tail[1],
            tail[2],
        ]
        .map(OsString::from);
        assert_eq!(request(&args).unwrap(), None);
        assert!(
            Options::parse(args)
                .err()
                .unwrap()
                .contains("third-party harness import was removed")
        );
        let peer = [
            "example.test",
            "--load-harness",
            "ssmcp",
            "--",
            "--endpoint",
            "https://peer.example/alice",
            "--token-env",
            "--version",
        ]
        .map(OsString::from);
        assert_eq!(request(&peer).unwrap(), None);
        assert!(
            Options::parse(peer)
                .err()
                .unwrap()
                .contains("peer option requires a nonempty value")
        );
    }

    #[test]
    fn version_requests_are_top_level_and_plain_output_is_unchanged() {
        let parse = |args: &[&str]| request(&args.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(parse(&["--version"]).unwrap(), Some(Request::Plain));
        assert_eq!(parse(&["-V"]).unwrap(), Some(Request::Plain));
        for args in [["--version", "--all"], ["--all", "-V"]] {
            assert_eq!(parse(&args).unwrap(), Some(Request::Dependencies));
        }
        assert!(parse(&["--all"]).is_err());
        assert!(parse(&["--version", "connect"]).is_err());
        assert_eq!(parse(&["wait", "--all"]).unwrap(), None);
        assert_eq!(parse(&["--", "--all"]).unwrap(), None);
        assert_eq!(
            render(Request::Plain).unwrap(),
            concat!("ssmcp ", env!("CARGO_PKG_VERSION"), "\n")
        );
    }

    #[test]
    fn actual_locked_root_resolves_every_reachable_edge() {
        let lock: Lock = toml::from_str(LOCK).unwrap();
        let mut tree = Tree::new(&lock.package).unwrap();
        let root = tree.resolve("ssmcp").unwrap();
        let mut independent = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(index) = pending.pop() {
            if independent.insert(index) {
                for edge in &lock.package[index].dependencies {
                    pending.push(tree.resolve(edge).unwrap());
                }
            }
        }
        let mut out = String::new();
        tree.append(&mut out, root, "").unwrap();
        assert_eq!(tree.expanded, independent);
        let rendered = render(Request::Dependencies).unwrap();
        assert!(rendered.ends_with(&out));
        assert!(dependencies(LOCK, "0.0.0-incorrect").is_err());
    }

    #[test]
    fn locked_identity_distinguishes_sources_and_versions_and_marks_shared_nodes_and_cycles() {
        let lock = r#"
[[package]]
name = "ssmcp"
version = "1.0.0"
dependencies = ["same 1.0.0 (registry+example)", "same 1.0.0 (git+example#1234)", "same 2.0.0"]
[[package]]
name = "same"
version = "1.0.0"
source = "registry+example"
dependencies = ["shared"]
[[package]]
name = "same"
version = "1.0.0"
source = "git+example#1234"
dependencies = ["shared"]
[[package]]
name = "same"
version = "2.0.0"
[[package]]
name = "shared"
version = "3.0.0"
dependencies = ["ssmcp"]
"#;
        let result = dependencies(lock, "1.0.0").unwrap();
        assert!(result.contains("same 1.0.0 (registry+example)"));
        assert!(result.contains("same 1.0.0 (git+example#1234)"));
        assert!(result.contains("same 2.0.0 (workspace/path)"));
        assert_eq!(
            result
                .matches("shared 3.0.0 (workspace/path) [already expanded]")
                .count(),
            1
        );
        assert!(result.contains("ssmcp 1.0.0 (workspace/path) [cycle]"));
        assert!(
            dependencies(
                &lock.replace("same 1.0.0 (git+example#1234)", "same 1.0.0"),
                "1.0.0"
            )
            .unwrap_err()
            .contains("ambiguous")
        );
        assert!(
            dependencies(&lock.replace("same 2.0.0", "same 9.9.9"), "1.0.0")
                .unwrap_err()
                .contains("missing")
        );
        assert!(
            dependencies(
                &lock.replace("git+example#1234", "registry+example"),
                "1.0.0"
            )
            .unwrap_err()
            .contains("duplicate")
        );
    }
}
