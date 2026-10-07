use std::path::Path;
use std::process::Command;

fn git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies the manifest directory"),
    );
    // An unpacked or vendored crate can sit inside somebody else's repository.
    // Only the tracked AFT workspace is a source of AFT build revisions.
    let workspace_root = manifest_dir.join("../..").canonicalize().ok();
    let git_root = git(&manifest_dir, &["rev-parse", "--show-toplevel"])
        .and_then(|root| Path::new(&root).canonicalize().ok());
    if workspace_root.is_none()
        || workspace_root != git_root
        || git(
            &manifest_dir,
            &[
                "ls-files",
                "--error-unmatch",
                "--",
                ":(top)crates/aft/Cargo.toml",
            ],
        )
        .is_none()
    {
        return;
    }
    // Watch the actual Git metadata paths, including linked worktrees, so a
    // new commit cannot leave the binary reporting a cached build revision.
    for name in ["HEAD", "packed-refs"] {
        if let Some(path) = git(&manifest_dir, &["rev-parse", "--git-path", name]) {
            if manifest_dir.join(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    if let Some(reference) = git(&manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&manifest_dir, &["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(revision) = git(&manifest_dir, &["rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=AFT_BUILD_GIT_SHA={revision}");
    }
}
