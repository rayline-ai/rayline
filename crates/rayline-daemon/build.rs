fn main() {
    println!("cargo:rerun-if-env-changed=RAYLINE_VERSION");
    println!("cargo:rerun-if-env-changed=RAYLINE_CHANNEL");

    let channel = std::env::var("RAYLINE_CHANNEL").unwrap_or_else(|_| "local".to_owned());
    println!("cargo:rustc-env=RAYLINE_DAEMON_CHANNEL={channel}");

    let version = std::env::var("RAYLINE_VERSION")
        .ok()
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set"));
    println!("cargo:rustc-env=RAYLINE_DAEMON_VERSION={version}");
}
