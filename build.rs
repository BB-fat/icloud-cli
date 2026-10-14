fn main() {
    println!(
        "cargo:rustc-env=ICLOUD_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
