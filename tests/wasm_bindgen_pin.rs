//! `wasm-bindgen-cli` must be exactly the `wasm-bindgen` version in
//! `wasm/Cargo.lock`, or the generated glue does not match the module. The
//! version is hard-coded in both workflows, so this test is what notices a
//! `cargo update` that moved one without the other.

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

/// The `version` of the `[[package]]` named `wasm-bindgen` in the lockfile.
fn locked_version() -> String {
    let lock = read("wasm/Cargo.lock");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line == "name = \"wasm-bindgen\"" {
            let v = lines.next().expect("version line after the name");
            return v
                .strip_prefix("version = \"")
                .and_then(|v| v.strip_suffix('"'))
                .expect("a quoted version")
                .to_string();
        }
    }
    panic!("wasm/Cargo.lock has no wasm-bindgen package");
}

/// Every version token following `marker`, up to the next non-version char.
fn versions_after(text: &str, marker: &str) -> Vec<String> {
    text.split(marker)
        .skip(1)
        .map(|rest| {
            rest.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect()
        })
        .collect()
}

#[test]
fn workflows_pin_wasm_bindgen_cli_to_the_locked_version() {
    let locked = locked_version();
    // The install command, the cache key and the "already installed?" check
    // each spell the version; all of them must follow the lockfile.
    let markers = [
        "wasm-bindgen-cli --version ",
        "wasm-bindgen-cli-${{ runner.os }}-",
        "!= \"wasm-bindgen ",
    ];
    for wf in [".github/workflows/release.yaml", ".github/workflows/ci.yaml"] {
        let text = read(wf);
        for marker in markers {
            let found = versions_after(&text, marker);
            assert!(!found.is_empty(), "{wf}: no `{marker}<version>` found");
            for v in found {
                assert_eq!(
                    v, locked,
                    "{wf}: `{marker}{v}` differs from wasm/Cargo.lock ({locked})"
                );
            }
        }
    }
}
