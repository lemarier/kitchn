//! Records the compilation target so `update` can select the matching release archive.

fn main() {
    // Cargo always sets TARGET for build scripts; `env!` in the crate fails the build otherwise.
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=KITCHEN_TARGET={target}");
    }
}
