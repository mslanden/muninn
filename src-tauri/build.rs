use std::path::Path;
use std::process::Command;

/// Compile the on-device speech transcriber helper (speech/transcriber.swift) into
/// binaries/muninn-transcriber-<triple> so tauri bundles it as the `externalBin`
/// sidecar declared in tauri.conf.json. The SpeechAnalyzer (macOS 26) code path is
/// compile-gated on the installed SDK — older SDKs (e.g. CI runners) still build the
/// SFSpeechRecognizer fallback. A failed compile keeps a previously built binary and
/// only warns; it panics only when no helper exists at all, because tauri_build would
/// then fail on the missing externalBin anyway.
fn compile_transcriber(target: &str) {
    let arch = if target.starts_with("aarch64") { "arm64" } else { "x86_64" };
    let out = Path::new("binaries").join(format!("muninn-transcriber-{target}"));
    let _ = std::fs::create_dir_all("binaries");

    let sdk_major: u32 = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .split('.')
                .next()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0);

    let mut args: Vec<String> = Vec::new();
    if sdk_major >= 26 {
        args.extend(["-D".into(), "HAVE_SPEECHANALYZER".into()]);
    }
    args.extend([
        "-O".into(),
        "-swift-version".into(),
        "5".into(),
        "-parse-as-library".into(),
        "-target".into(),
        format!("{arch}-apple-macosx12.0"),
        "speech/transcriber.swift".into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
    ]);

    match Command::new("xcrun").arg("swiftc").args(&args).status() {
        Ok(s) if s.success() => {}
        _ if out.is_file() => {
            println!("cargo:warning=swiftc failed; keeping the previously built muninn-transcriber");
        }
        _ => panic!(
            "failed to compile speech/transcriber.swift — the muninn-transcriber sidecar is \
             required by tauri.conf.json externalBin. Is Xcode (or the Command Line Tools) installed?"
        ),
    }
}

/// Compile the on-device face-detection helper (speech/framescan.swift) into
/// binaries/muninn-framescan-<triple> so tauri bundles it as the `externalBin` sidecar
/// declared in tauri.conf.json. Unlike the transcriber it uses only plain Vision +
/// AVFoundation APIs (macOS 10.15+), so there is no SDK gating. Same keep-stale/fail-if-
/// missing semantics: a failed compile keeps a previously built binary and only warns; it
/// panics only when no helper exists at all (tauri_build would otherwise fail on the
/// missing externalBin).
fn compile_framescan(target: &str) {
    let arch = if target.starts_with("aarch64") { "arm64" } else { "x86_64" };
    let out = Path::new("binaries").join(format!("muninn-framescan-{target}"));
    let _ = std::fs::create_dir_all("binaries");

    let args: Vec<String> = vec![
        "-O".into(),
        "-swift-version".into(),
        "5".into(),
        "-parse-as-library".into(),
        "-target".into(),
        format!("{arch}-apple-macosx12.0"),
        "speech/framescan.swift".into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
    ];

    match Command::new("xcrun").arg("swiftc").args(&args).status() {
        Ok(s) if s.success() => {}
        _ if out.is_file() => {
            println!("cargo:warning=swiftc failed; keeping the previously built muninn-framescan");
        }
        _ => panic!(
            "failed to compile speech/framescan.swift — the muninn-framescan sidecar is \
             required by tauri.conf.json externalBin. Is Xcode (or the Command Line Tools) installed?"
        ),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=Entitlements.plist");
    println!("cargo:rerun-if-changed=Info.plist");
    println!("cargo:rerun-if-changed=speech/transcriber.swift");
    println!("cargo:rerun-if-changed=speech/framescan.swift");

    let target = std::env::var("TARGET").unwrap_or_default();
    // Lets video.rs locate the triple-suffixed helpers in dev/test runs.
    println!("cargo:rustc-env=MUNINN_TARGET_TRIPLE={target}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        compile_transcriber(&target);
        compile_framescan(&target);
    }
    tauri_build::build();
}
