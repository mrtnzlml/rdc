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
const FILE_TYPE_NAMES: &[&str] = &["ft", "meta", "file_type", "ty"];

/// Names too common to trust alone. Exempt only when the same or the previous
/// line shows where the binding came from: `file_type()` or a vfs `metadata`.
const FILE_TYPE_NAMES_IN_CONTEXT: &[&str] = &["t", "m"];

fn is_file_type_receiver(before: &str, context: &str) -> bool {
    let before = before.trim_end_matches(['?']);
    if before.ends_with("file_type()") || before.ends_with("file_type().ok()") {
        return true;
    }
    let name: String = before.chars().rev().take_while(|c| is_word(*c)).collect::<Vec<_>>().into_iter().rev().collect();
    FILE_TYPE_NAMES.contains(&name.as_str())
        || (FILE_TYPE_NAMES_IN_CONTEXT.contains(&name.as_str())
            && (context.contains("file_type()") || context.contains("metadata(")))
}

/// True when `line` calls `needle` on something that is not a file type.
/// `prev` is the line before it, where a closure parameter or `let` may have
/// been bound.
fn flags(prev: &str, line: &str, needle: &str) -> bool {
    let method = needle == ".is_dir()" || needle == ".is_file()";
    let context = format!("{prev} {line}");
    line.match_indices(needle).any(|(i, _)| !(method && is_file_type_receiver(&line[..i], &context)))
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

/// `src` with every comment, string literal (raw ones included) and char
/// literal blanked to spaces, newlines kept, so line numbers still match and a
/// `{` or `std::fs` inside text is invisible to the scans below.
fn mask(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let mut out: Vec<char> = c.clone();
    let blank = |out: &mut Vec<char>, from: usize, to: usize| {
        for ch in &mut out[from..to] {
            if *ch != '\n' {
                *ch = ' ';
            }
        }
    };
    let mut i = 0;
    while i < c.len() {
        let rest = &c[i..];
        if rest.starts_with(&['/', '/']) {
            let end = c[i..].iter().position(|&x| x == '\n').map_or(c.len(), |p| i + p);
            blank(&mut out, i, end);
            i = end;
        } else if rest.starts_with(&['/', '*']) {
            let (mut depth, mut j) = (0, i);
            while j < c.len() {
                if c[j..].starts_with(&['/', '*']) {
                    depth += 1;
                    j += 2;
                } else if c[j..].starts_with(&['*', '/']) {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if let Some(hashes) = raw_string_open(&c, i) {
            // r"…", r#"…"#, br"…": no escapes, ends at `"` plus the same hashes.
            let body = i + c[i..].iter().position(|&x| x == '"').unwrap() + 1;
            let close: Vec<char> = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
            let end = (body..c.len()).find(|&j| c[j..].starts_with(&close)).map_or(c.len(), |j| j + close.len());
            blank(&mut out, i, end);
            i = end;
        } else if c[i] == '"' {
            let mut j = i + 1;
            while j < c.len() && c[j] != '"' {
                j += if c[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(c.len());
            blank(&mut out, i, end);
            i = end;
        } else if c[i] == '\'' {
            // A char literal, as opposed to a lifetime or a label.
            let end = if c.get(i + 1) == Some(&'\\') {
                c[i + 2..].iter().position(|&x| x == '\'').map(|p| i + 2 + p + 1)
            } else if c.get(i + 2) == Some(&'\'') {
                Some(i + 3)
            } else {
                None
            };
            match end {
                Some(end) => {
                    blank(&mut out, i, end);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// If a raw string literal (`r"`, `r#"`, `br"`) opens at `i`, its hash count.
fn raw_string_open(c: &[char], i: usize) -> Option<usize> {
    if i > 0 && is_word(c[i - 1]) {
        return None;
    }
    let mut j = i;
    if c[j] == 'b' {
        j += 1;
    }
    if c.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = c[j..].iter().take_while(|&&x| x == '#').count();
    (c.get(j + hashes) == Some(&'"')).then_some(hashes)
}

/// Where the item a `#[cfg(test)]` attribute applies to ends, scanning `c`
/// from just after the attribute: an item with a body ends at its closing `}`;
/// a brace-less item or an initializer at its `;` or `,`; a last field or
/// variant at the `}` that closes its parent, which is not part of it.
fn item_end(c: &[char], from: usize) -> usize {
    let (mut depth, mut angle, mut opened) = (0usize, 0usize, false);
    for (j, &ch) in c.iter().enumerate().skip(from) {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth == 0 {
                    return j;
                }
                depth -= 1;
            }
            '{' => {
                opened |= depth == 0;
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    return j;
                }
                depth -= 1;
                if depth == 0 && opened {
                    return j + 1;
                }
            }
            '<' => angle += 1,
            '>' if !matches!(c[j - 1], '-' | '=') => angle = angle.saturating_sub(1),
            ';' if depth == 0 => return j + 1,
            ',' if depth == 0 && angle == 0 && !opened => return j + 1,
            _ => {}
        }
    }
    c.len()
}

/// The production lines of `src`, masked (see [`mask`]) and with every
/// `#[cfg(test)]` attribute and the one item it applies to blanked out. One
/// entry per source line, so index + 1 is the line number.
fn production_lines(src: &str) -> Vec<String> {
    const ATTR: &str = "#[cfg(test)]";
    let mut c: Vec<char> = mask(src).chars().collect();
    let attr: Vec<char> = ATTR.chars().collect();
    let mut i = 0;
    while i + attr.len() <= c.len() {
        if c[i..].starts_with(&attr) {
            let end = item_end(&c, i + attr.len());
            for ch in &mut c[i..end] {
                if *ch != '\n' {
                    *ch = ' ';
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    c.into_iter().collect::<String>().lines().map(str::to_string).collect()
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
        let masked = production_lines(&src);
        let original: Vec<&str> = src.lines().collect();
        for (i, line) in masked.iter().enumerate() {
            let prev = if i > 0 { masked[i - 1].as_str() } else { "" };
            if mentions_std_fs(line) || NEEDLES.iter().any(|p| flags(prev, line, p)) {
                hits.push(format!("{rel}:{}: {}", i + 1, original[i].trim()));
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

/// The needles a scan of `src` flags, for the scanner's own tests.
fn flagged(src: &str) -> Vec<String> {
    let masked = production_lines(src);
    let mut out = Vec::new();
    for (i, line) in masked.iter().enumerate() {
        let prev = if i > 0 { masked[i - 1].as_str() } else { "" };
        if mentions_std_fs(line) || NEEDLES.iter().any(|p| flags(prev, line, p)) {
            out.push(line.trim().to_string());
        }
    }
    out
}

#[test]
fn braces_in_literals_and_comments_are_ignored() {
    let src = r##"
#[cfg(test)]
mod tests {
    const A: &str = "}";
    const B: &str = r"}";
    const C: &str = r#"} "quoted" }"#;
    const D: char = '}';
    // }
    /* } /* nested } */ } */
    fn t() { let _ = '\'; }
}
fn production() { p.exists(); }
"##;
    assert_eq!(flagged(src), ["fn production() { p.exists(); }"]);
}

#[test]
fn needles_in_literals_and_comments_are_not_flagged() {
    let src = r#"
fn f<'a>(x: &'a str) {
    let _ = "p.exists() and std::fs";
    // p.exists()
    let _ = '{'; /* std::fs */
}
"#;
    assert!(flagged(src).is_empty(), "{:?}", flagged(src));
}

#[test]
fn production_after_a_mid_file_test_module_is_still_scanned() {
    let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { std::fs::read(\"x\"); }\n}\nfn b() { std::fs::read(\"y\"); }\n";
    assert_eq!(flagged(src), ["fn b() { std::fs::read(   ); }"]);
}

#[test]
fn cfg_test_on_a_field_a_variant_and_an_initializer_skips_only_that() {
    let src = "struct S {\n    a: u8,\n    #[cfg(test)]\n    f: Option<std::time::SystemTime>,\n    b: u8,\n}\nfn f() { p.exists(); }\n";
    assert_eq!(flagged(src), ["fn f() { p.exists(); }"]);
    // Last field, no trailing comma: the parent's `}` is not swallowed.
    let src = "struct S {\n    #[cfg(test)]\n    f: std::fs::File\n}\nfn f() { p.exists(); }\n";
    assert_eq!(flagged(src), ["fn f() { p.exists(); }"]);
    let src = "enum E {\n    A,\n    #[cfg(test)]\n    B(std::fs::File),\n    C,\n}\nfn f() { p.exists(); }\n";
    assert_eq!(flagged(src), ["fn f() { p.exists(); }"]);
    let src = "fn f() -> S {\n    S {\n        a: 1,\n        #[cfg(test)]\n        f: std::fs::read(\"x\"),\n        b: p.exists(),\n    }\n}\n";
    assert_eq!(flagged(src), ["b: p.exists(),"]);
}

#[test]
fn cfg_test_item_with_generics_and_braceless_forms() {
    let src = "#[cfg(test)]\nuse std::{fs, io};\n#[cfg(test)]\nfn g<A, B>(a: A, b: B) { std::fs::read(\"x\"); }\nfn f() { p.exists(); }\n";
    assert_eq!(flagged(src), ["fn f() { p.exists(); }"]);
}

#[test]
fn ambiguous_receivers_need_a_file_type_context() {
    assert!(flagged("let t = e.file_type()?;\nif t.is_dir() {}\n").is_empty());
    assert!(flagged("e.file_type().is_ok_and(|t| t.is_dir())\n").is_empty());
    assert!(flagged("vfs::metadata(p).is_ok_and(|m| m.is_dir())\n").is_empty());
    assert_eq!(flagged("let t = p;\nif t.is_dir() {}\n"), ["if t.is_dir() {}"]);
}
