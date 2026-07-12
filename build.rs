fn main() {
    // XXX: This build script only runs *outside* Bazel.
    println!("cargo:rerun-if-env-changed=BUBBLEWRAP_PATH");

    if let Ok(bubblewrap_path) = std::env::var("BUBBLEWRAP_PATH") {
        // Pass it through, it was already set by the user.
        println!("cargo:rustc-env=BUBBLEWRAP_PATH={bubblewrap_path}");
    } else {
        if std::env::var_os("CARGO_FEATURE_EXTERNAL_BWRAP").is_some() {
            panic!(
                "The external-bwrap feature requires BUBBLEWRAP_PATH to be set to the runtime path of a bubblewrap binary."
            );
        }

        // See if we're in the Bazel workspace.
        if std::fs::metadata("MODULE.bazel").is_ok() {
            println!("cargo:rerun-if-changed=MODULE.bazel");
            for file in walkdir::WalkDir::new("third_party/modules/bubblewrap") {
                let file = file.expect("Failed to read file");
                if file.file_type().is_file() {
                    println!("cargo:rerun-if-changed={}", file.path().display());
                }
            }

            // We're in the Bazel workspace.  Build and get the path to the
            // hermetic bwrap binary.
            let result = std::process::Command::new("bazel")
                .args(["build", "@bubblewrap"])
                .status()
                .expect("Failed to run Bazel build for bwrap");
            if !result.success() {
                panic!("Bazel build for bwrap failed");
            }

            // Get the path to the built binary.
            let output = std::process::Command::new("bazel")
                .args(["--quiet", "cquery", "@bubblewrap", "--output=files"])
                .output()
                .expect("Failed to run Bazel cquery for bwrap");
            let lines =
                String::from_utf8(output.stdout).expect("Failed to parse Bazel cquery output");
            let lines = lines.lines().collect::<Vec<_>>();
            if lines.len() != 1 {
                panic!(
                    "Bazel cquery for bwrap returned unexpected number of lines: {}",
                    lines.len()
                );
            }

            let bubblewrap_path =
                std::fs::canonicalize(lines[0]).expect("Failed to canonicalize bwrap path");
            println!(
                "cargo:rustc-env=BUBBLEWRAP_PATH={}",
                bubblewrap_path.display()
            );
        } else {
            panic!(
                "Please set the BUBBLEWRAP_PATH environment variable to the path of a bubblewrap binary."
            );
        }
    }
}
