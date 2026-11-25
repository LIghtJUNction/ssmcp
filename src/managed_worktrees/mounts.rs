//! Git can see a clean checkout when part of it is a mount presenting the same
//! files. Recursive worktree removal must never descend into such a mount.
use super::*;
#[cfg(target_os = "linux")]
pub(super) fn check(tree: &Path, _deadline: Instant) -> io::Result<()> {
    let table = bounded_path(Path::new("/proc/self/mountinfo"), 4 * 1024 * 1024)?;
    let mut root = Vec::new();
    for &byte in tree.as_os_str().as_encoded_bytes() {
        match byte {
            b' ' => root.extend_from_slice(br"\040"),
            b'\t' => root.extend_from_slice(br"\011"),
            b'\n' => root.extend_from_slice(br"\012"),
            b'\\' => root.extend_from_slice(br"\134"),
            byte => root.push(byte),
        }
    }
    ensure(!table.is_empty(), "cannot verify mount table")?;
    for row in table.split(|&b| b == b'\n').filter(|s| !s.is_empty()) {
        let mount = row
            .split(|&b| b == b' ')
            .nth(4)
            .ok_or_else(|| io::Error::other("incomplete mount table"))?;
        if let Some(tail) = mount.strip_prefix(root.as_slice()) {
            ensure(
                !tail.is_empty() && tail[0] != b'/',
                "checkout contains a mount",
            )?;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn check(tree: &Path, deadline: Instant) -> io::Result<()> {
    let parent = tree
        .parent()
        .ok_or_else(|| io::Error::other("missing worktree parent"))?;
    let device = fs::metadata(parent)?.dev();
    let mut pending = vec![tree.to_owned()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        ensure(Instant::now() < deadline, "mount inspection timed out")?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure(
            metadata.dev() == device,
            "checkout crosses filesystem boundary",
        )?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visited += 1;
                ensure(visited < 32_768, "mount inspection limit exceeded")?;
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}
