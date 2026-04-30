fn main() {
    // tauri_build doesn't watch icons/, so an icon swap won't trigger a rebuild
    // and the embedded icon stays stale. Watch it explicitly.
    println!("cargo:rerun-if-changed=icons");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.png");
    tauri_build::build()
}
