//! Inspect archives in memory. Archive paths are never used as write targets.
use super::release::{self, Release};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

pub(super) const MAX_EXECUTABLE: usize = 64 * 1024 * 1024;
const MAX_EXPANDED: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;

fn read_limited(reader: impl Read, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Invalid Zuse archive: {error}"))?;
    if bytes.len() > limit {
        return Err("Zuse archive exceeds its expanded byte limit".into());
    }
    Ok(bytes)
}

fn relative<'a>(name: &'a str, prefix: &str) -> Result<&'a str, String> {
    if name.len() > 512
        || !name.is_ascii()
        || name.bytes().any(|byte| byte.is_ascii_control())
        || name.contains(['\\', ':'])
        || name
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err("Zuse archive contains an unsafe member path".into());
    }
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or_else(|| "Zuse archive member is outside the exact release directory".into())
}

fn member(name: &str, release: &Release, mode: u32) -> Result<bool, String> {
    let relative = relative(name, &release.prefix())?;
    if mode & 0o7000 != 0 {
        return Err("Zuse archive contains special permission bits".into());
    }
    let executable = relative == release::executable_name(&release.target);
    if executable {
        if mode & 0o111 == 0 {
            return Err("Zuse archive executable has no executable permission".into());
        }
        return Ok(true);
    }
    let document = (!relative.contains('/')
        && ((relative.starts_with("README") && relative.ends_with(".md"))
            || matches!(relative, "LICENSE" | "CHANGELOG.md")))
        || relative
            .strip_prefix("docs/")
            .is_some_and(|name| !name.contains('/') && name.ends_with(".md"))
        || relative
            .strip_prefix("examples/")
            .is_some_and(|name| !name.contains('/') && name.ends_with(".zuse"));
    if !document || mode & 0o111 != 0 {
        return Err(format!("Unexpected member in the Zuse release archive: {relative}"));
    }
    Ok(false)
}

fn tar(bytes: &[u8], release: &Release) -> Result<Vec<u8>, String> {
    let mut decoder = flate2::bufread::GzDecoder::new(bytes);
    let expanded = read_limited(&mut decoder, MAX_EXPANDED)?;
    if !decoder.into_inner().is_empty() {
        return Err("Zuse gzip archive contains trailing data or another stream".into());
    }
    let mut archive = tar::Archive::new(Cursor::new(&expanded));
    let mut seen = BTreeSet::new();
    let mut executable = None;
    // Raw mode exposes PAX/GNU extension entries rather than silently applying
    // a second path or size. The official release uses plain regular entries.
    for entry in archive.entries().map_err(|error| error.to_string())?.raw(true) {
        let mut entry = entry.map_err(|error| error.to_string())?;
        if seen.len() >= MAX_ENTRIES || !entry.header().entry_type().is_file() {
            return Err("Zuse tar archive contains too many entries or a non-regular entry".into());
        }
        let name = std::str::from_utf8(entry.path_bytes().as_ref())
            .map_err(|_| "Zuse archive member name is not UTF-8")?
            .to_owned();
        if !seen.insert(name.clone()) {
            return Err("Zuse archive contains a duplicate member".into());
        }
        let is_executable = member(
            &name,
            release,
            entry.header().mode().map_err(|error| error.to_string())?,
        )?;
        let size = entry.size();
        if size > MAX_EXECUTABLE as u64 {
            return Err("Zuse archive member is too large".into());
        }
        let content = read_limited(&mut entry, size as usize)?;
        if content.len() as u64 != size {
            return Err("Zuse tar member is truncated".into());
        }
        if is_executable {
            executable = Some(content);
        }
    }
    let consumed = archive.into_inner().position() as usize;
    if expanded[consumed..].iter().any(|byte| *byte != 0) {
        return Err("Zuse tar archive contains data after its terminator".into());
    }
    executable.ok_or_else(|| "Zuse archive has no expected executable".into())
}

fn zip(bytes: &[u8], release: &Release) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("Invalid Zuse ZIP archive: {error}"))?;
    if archive.is_empty()
        || archive.len() > MAX_ENTRIES
        || archive.offset() != 0
        || !archive.comment().is_empty()
        || archive.zip64_comment().is_some()
        || archive.has_overlapping_files().map_err(|error| error.to_string())?
    {
        return Err("Zuse ZIP archive has an unsupported or ambiguous layout".into());
    }
    let mut expected = BTreeMap::new();
    let mut executable = None;
    let mut expanded = 0usize;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let mode = entry.unix_mode().ok_or("Zuse ZIP member has no regular-file mode")?;
        if entry.encrypted() || !entry.is_file() || mode & 0o170000 != 0o100000 {
            return Err("Zuse ZIP contains an encrypted or non-regular entry".into());
        }
        let name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| "Zuse ZIP member name is not UTF-8")?
            .to_owned();
        let is_executable = member(&name, release, mode)?;
        if entry.size() > MAX_EXPANDED.saturating_sub(expanded) as u64 {
            return Err("Zuse ZIP archive exceeds its expanded byte limit".into());
        }
        let declared = entry.size();
        // Reading to EOF also checks the library's CRC32 verifier.
        let content = read_limited(&mut entry, declared as usize)?;
        if content.len() as u64 != declared {
            return Err("Zuse ZIP member size differs from its directory".into());
        }
        expanded += content.len();
        if expected
            .insert(name, (declared, release::digest(&content)))
            .is_some()
        {
            return Err("Zuse ZIP archive contains a duplicate member".into());
        }
        if is_executable {
            executable = Some(content);
        }
    }
    // ZipArchive indexes central-directory names. Independently walk the local
    // entries with the library's stream reader so duplicate or hidden local
    // members cannot disappear through that name index.
    let mut stream = Cursor::new(bytes);
    let mut seen = BTreeSet::new();
    while let Some(mut entry) =
        zip::read::read_zipfile_from_stream(&mut stream).map_err(|error| error.to_string())?
    {
        if seen.len() >= MAX_ENTRIES {
            return Err("Zuse ZIP contains too many local entries".into());
        }
        let name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| "Zuse ZIP member name is not UTF-8")?
            .to_owned();
        if !seen.insert(name.clone()) {
            return Err("Zuse ZIP contains duplicate local entries".into());
        }
        let (size, sha256) = expected
            .get(&name)
            .ok_or("Zuse ZIP contains a local entry absent from its directory")?;
        let content = read_limited(&mut entry, *size as usize)?;
        if content.len() as u64 != *size || release::digest(&content) != *sha256 {
            return Err("Zuse ZIP local entry differs from its directory".into());
        }
    }
    if seen.len() != expected.len() {
        return Err("Zuse ZIP directory contains an absent local entry".into());
    }
    executable.ok_or_else(|| "Zuse archive has no expected executable".into())
}

fn executable_target(bytes: &[u8], target: &str) -> bool {
    if target.ends_with("-unknown-linux-gnu") {
        let machine = if target.starts_with("aarch64-") { 183u16 } else { 62u16 };
        return bytes.len() >= 64
            && bytes.starts_with(b"\x7fELF")
            && bytes[4] == 2
            && bytes[5] == 1
            && u16::from_le_bytes([bytes[18], bytes[19]]) == machine;
    }
    if target.ends_with("-apple-darwin") {
        let cpu = if target.starts_with("aarch64-") { 0x0100000cu32 } else { 0x01000007u32 };
        return bytes.len() >= 32
            && bytes[..4] == [0xcf, 0xfa, 0xed, 0xfe]
            && u32::from_le_bytes(bytes[4..8].try_into().unwrap()) == cpu;
    }
    if target == "x86_64-pc-windows-msvc" && bytes.len() >= 64 && bytes.starts_with(b"MZ") {
        let offset = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
        return bytes.get(offset..offset.saturating_add(6))
            .is_some_and(|header| header == b"PE\0\0\x64\x86");
    }
    false
}

pub(super) fn executable(bytes: &[u8], release: &Release, target: &str) -> Result<Vec<u8>, String> {
    release.verify(bytes, target)?;
    let executable = if target.ends_with("-windows-msvc") {
        zip(bytes, release)?
    } else {
        tar(bytes, release)?
    };
    if executable.is_empty()
        || executable.len() > MAX_EXECUTABLE
        || !executable_target(&executable, target)
    {
        return Err("Zuse executable does not match the release platform and architecture".into());
    }
    Ok(executable)
}
