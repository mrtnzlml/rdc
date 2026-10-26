//! Every file access in production code goes through `rdc::vfs`, so the
//! browser build can swap in an in-memory tree. A direct `std::fs` call (or a
//! `Path::exists()`-style check, which hits the disk too) would silently bypass
//! it. `#[cfg(test)]` items stay on `std::fs`.

use std::path::{Path, PathBuf};

/// Files allowed to touch the real disk. `vfs.rs` is the implementation,
/// `upgrade.rs` and `main.rs` are native-only, and `api/retry.rs`'s HTTP-trace
/// sink appends to a file named by an environment variable and is never
/// active in the browser.
const ALLOWED: &[&str] = &["src/vfs.rs", "src/upgrade.rs", "src/main.rs", "src/api/retry.rs"];

const NEEDLES: &[&str] = &[".exists()", ".is_dir()", ".is_file()", "OpenOptions", "DirBuilder",
    ".symlink_metadata()", ".metadata()", ".canonicalize()", ".read_dir(",
];

/// `vfs::FileType` has `is_dir()` / `is_file()` methods, like `std::fs::FileType`.
/// Text cannot tell them from `Path::is_dir()`, so a receiver that is plainly a
/// file type is exempt: a `file_type()` call, or one of these local names.
const FILE_TYPE_NAMES: &[&str] = &["t", "ft", "m", "meta", "file_type", "ty"];

fn is_file_type_receiver(before: &str) -> bool {
    let before = before.trim_end_matches(['?']);
    if before.ends_with("file_type()") || before.ends_with("file_type().ok()") {
        return true;
    }
    let name: String = before.chars().rev().take_while(|c| is_word(*c)).collect::<Vec<_>>().into_iter().rev().collect();
    FILE_TYPE_NAMES.contains(&name.as_str())
}

/// True when `line` calls `needle` on something that is not a file type.
fn flags(line: &str, needle: &str) -> bool {
    let method = needle == ".is_dir()" || needle == ".is_file()";
    line.match_indices(needle).any(|(i, _)| !(method && is_file_type_receiver(&line[..i])))
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `line` names `std::fs` as a whole path segment.
fn mentions_std_fs(line: &str) -> bool {
    let mut rest = line;
    while let Some(i) = rest.find("std::fs") {
        let after = &rest[i + "std::fs".len()..];
        if !after.chars().next().is_some_and(is_word) {
            return true;
        }
        rest = after;
    }
    false
}

fn brace_delta(line: &str) -> (i32, bool) {
    let mut delta = 0;
    let mut seen = false;
    for c in line.chars() {
        match c {
            '{' => {
                delta += 1;
                seen = true;
            }
            '}' => {
                delta -= 1;
                seen = true;
            }
            _ => {}
        }
    }
    (delta, seen)
}

/// The production lines of `src`, as (1-based line number, text): everything
/// except each `#[cfg(test)]` attribute plus the item after it (brace-balanced,
/// or up to the `;` for a brace-less item), and except comment lines.
fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut lines = src.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let t = line.trim_start();
        if t.starts_with("#[cfg(test)]") {
            let (mut depth, mut opened) = (0, false);
            for (_, item) in lines.by_ref() {
                let it = item.trim_start();
                if it.starts_with("#[") || it.starts_with("//") {
                    continue;
                }
                let (d, seen) = brace_delta(item);
                depth += d;
                opened |= seen;
                if (opened && depth <= 0) || (!opened && it.ends_with(';')) {
                    break;
                }
            }
            continue;
        }
        if t.starts_with("//") {
            continue;
        }
        out.push((n + 1, line));
    }
    out
}

#[test]
fn no_direct_fs_outside_vfs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    files.sort();
    let mut hits = Vec::new();
    for file in files {
        let rel = file.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if ALLOWED.contains(&rel.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(&file).unwrap();
        for (n, line) in production_lines(&src) {
            if mentions_std_fs(line) || NEEDLES.iter().any(|p| flags(line, p)) {
                hits.push(format!("{rel}:{n}: {}", line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "{} direct file access site(s) outside vfs:\n{}",
        hits.len(),
        hits.join("\n")
    );
}
