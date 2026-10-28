use std::path::Path;

use anyhow::Context;
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
pub struct CountObjectsResult {
    /// Number of loose objects: one compressed file each under `.git/objects/xx/`.
    pub loose_objects: u64,
    /// Disk space the loose objects take, in bytes (allocated blocks, as `git count-objects` measures it).
    pub loose_bytes: u64,
    /// Objects stored in pack files, counted per pack: an object in two packs counts twice, as in git.
    pub packed_objects: u64,
    /// Number of pack files.
    pub packs: u64,
    /// Total size of the pack files and their indexes, in bytes.
    pub pack_bytes: u64,
    /// True when a commit-graph file exists, which speeds up history walks.
    pub commit_graph: bool,
    /// True when a multi-pack index exists.
    pub multi_pack_index: bool,
    /// Number of other object directories this repository borrows objects from (`objects/info/alternates`).
    pub alternates: usize,
}

/// Object database statistics for this repository's own object directory, like `git count-objects -v`.
pub fn count_objects(repo: &gix::Repository) -> anyhow::Result<CountObjectsResult> {
    let objects = repo.objects.store_ref().path();
    let (loose_objects, loose_bytes) = count_loose(objects, repo.object_hash())?;
    let (packs, packed_objects, pack_bytes) =
        count_packs(&objects.join("pack"), repo.object_hash())?;
    let info = objects.join("info");
    Ok(CountObjectsResult {
        loose_objects,
        loose_bytes,
        packed_objects,
        packs,
        pack_bytes,
        commit_graph: info.join("commit-graph").is_file()
            || info.join("commit-graphs/commit-graph-chain").is_file(),
        multi_pack_index: objects.join("pack/multi-pack-index").is_file(),
        alternates: std::fs::read_to_string(info.join("alternates"))
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                    .count()
            })
            .unwrap_or(0),
    })
}

/// Loose objects live in `xx/yyyy…`: a directory per first byte, a file named after the rest of the hash.
fn count_loose(objects: &Path, hash: gix::hash::Kind) -> anyhow::Result<(u64, u64)> {
    let rest_len = hash.len_in_hex() - 2;
    let (mut count, mut bytes) = (0, 0);
    for dir in read_dir(objects)? {
        let dir = dir?;
        let name = dir.file_name();
        if !is_hex(&name, 2) || !dir.file_type()?.is_dir() {
            continue;
        }
        for file in read_dir(&dir.path())? {
            let file = file?;
            if is_hex(&file.file_name(), rest_len) {
                count += 1;
                bytes += disk_usage(&file.metadata()?);
            }
        }
    }
    Ok((count, bytes))
}

/// `(packs, objects, bytes)` over every pack with an index, as git counts them.
fn count_packs(pack_dir: &Path, hash: gix::hash::Kind) -> anyhow::Result<(u64, u64, u64)> {
    let (mut packs, mut objects, mut bytes) = (0, 0, 0);
    if !pack_dir.is_dir() {
        return Ok((0, 0, 0));
    }
    for entry in read_dir(pack_dir)? {
        let idx = entry?.path();
        let pack = idx.with_extension("pack");
        if idx.extension().is_none_or(|ext| ext != "idx") || !pack.is_file() {
            continue;
        }
        let index = gix::odb::pack::index::File::at(&idx, hash)
            .with_context(|| format!("cannot read pack index {}", idx.display()))?;
        packs += 1;
        objects += u64::from(index.num_objects());
        bytes += std::fs::metadata(&idx)?.len() + std::fs::metadata(&pack)?.len();
    }
    Ok((packs, objects, bytes))
}

fn read_dir(path: &Path) -> anyhow::Result<std::fs::ReadDir> {
    std::fs::read_dir(path).with_context(|| format!("cannot list {}", path.display()))
}

fn is_hex(name: &std::ffi::OsStr, len: usize) -> bool {
    name.to_str()
        .is_some_and(|s| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(unix)]
fn disk_usage(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // st_blocks is in 512-byte units on every unix, whatever the filesystem's block size.
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn disk_usage(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::tools::testutil::{
        bare_repo, empty_repo, fixture_repo, git_stdout, open, run_git, write,
    };

    /// `git count-objects -v` as a map, the oracle for count_objects. Sizes are in KiB.
    fn git_counts(dir: &Path) -> HashMap<String, u64> {
        git_stdout(dir, &["count-objects", "-v"])
            .lines()
            .filter_map(|line| line.split_once(": "))
            .filter_map(|(k, v)| Some((k.to_owned(), v.parse().ok()?)))
            .collect()
    }

    fn assert_matches_git(dir: &Path, res: &CountObjectsResult) {
        let git = git_counts(dir);
        assert_eq!(res.loose_objects, git["count"]);
        assert_eq!(res.loose_bytes / 1024, git["size"]);
        assert_eq!(res.packed_objects, git["in-pack"]);
        assert_eq!(res.packs, git["packs"]);
        assert_eq!(res.pack_bytes / 1024, git["size-pack"]);
    }

    #[test]
    fn loose_then_packed_match_git() {
        let (dir, repo) = fixture_repo();
        let root = dir.path();
        let res = count_objects(&repo).unwrap();
        assert_matches_git(root, &res);
        assert!(res.loose_objects > 0);
        assert_eq!((res.packs, res.packed_objects), (0, 0));
        assert!(!res.commit_graph && !res.multi_pack_index);

        run_git(root, &["gc", "-q"]);
        write(root, "new.txt", "loose again\n");
        run_git(root, &["add", "new.txt"]);
        run_git(root, &["commit-graph", "write", "--reachable"]);
        run_git(root, &["multi-pack-index", "write"]);
        let res = count_objects(&open(root)).unwrap();
        assert_matches_git(root, &res);
        assert_eq!(res.packs, 1);
        assert!(res.packed_objects > 0 && res.loose_objects == 1);
        assert!(res.commit_graph && res.multi_pack_index);
        assert_eq!(res.alternates, 0);
    }

    #[test]
    fn alternates_bare_and_empty() {
        let (src, _) = fixture_repo();
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("shared");
        run_git(
            dir.path(),
            &[
                "clone",
                "-q",
                "--shared",
                src.path().to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        let res = count_objects(&open(&clone)).unwrap();
        assert_eq!(res.alternates, 1);
        assert_eq!(res.loose_objects, 0, "a shared clone borrows every object");
        assert_matches_git(&clone, &res);

        let (dir, bare) = bare_repo();
        assert_matches_git(&dir.path().join("bare.git"), &count_objects(&bare).unwrap());
        let (_dir, empty) = empty_repo();
        let res = count_objects(&empty).unwrap();
        assert_eq!((res.loose_objects, res.packs), (0, 0));
    }

    #[test]
    fn hex_names() {
        assert!(is_hex("ab".as_ref(), 2));
        assert!(!is_hex("info".as_ref(), 2));
        assert!(!is_hex("zz".as_ref(), 2));
    }
}
