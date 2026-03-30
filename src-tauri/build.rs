fn main() {
    tauri_build::build();
    println!("cargo:rerun-if-changed=Entitlements.plist");
    println!("cargo:rerun-if-changed=Info.plist");
}
