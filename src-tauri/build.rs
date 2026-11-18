use std::path::Path;
use std::process::Command;

/// Compile a Swift helper into binaries/<name>-<triple> so tauri bundles it as an
/// `externalBin` sidecar (declared in tauri.conf.json). A failed compile keeps a previously
/// built binary and only warns; it panics only when no helper exists at all, because
/// tauri_build would then fail on the missing externalBin anyway.
fn compile_helper(target: &str, name: &str, source: &str, extra_flags: &[&str]) {
    let arch = if target.starts_with("aarch64") { "arm64" } else { "x86_64" };
    let out = Path::new("binaries").join(format!("{name}-{target}"));
    let _ = std::fs::create_dir_all("binaries");

    let mut args: Vec<String> = extra_flags.iter().map(|f| f.to_string()).collect();
    args.extend([
        "-O".into(),
        "-swift-version".into(),
        "5".into(),
        "-parse-as-library".into(),
        "-target".into(),
        format!("{arch}-apple-macosx12.0"),
        source.into(),
        "-o".into(),
        out.to_string_lossy().into_owned(),
    ]);

    match Command::new("xcrun").arg("swiftc").args(&args).status() {
        Ok(s) if s.success() => {}
        _ if out.is_file() => {
            println!("cargo:warning=swiftc failed; keeping the previously built {name}");
        }
        _ => panic!(
            "failed to compile {source} — the {name} sidecar is required by tauri.conf.json \
             externalBin. Is Xcode (or the Command Line Tools) installed?"
        ),
    }
}

/// Major version of the installed macOS SDK (0 if unknown).
fn sdk_major() -> u32 {
    Command::new("xcrun")
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
        .unwrap_or(0)
}

fn main() {
    println!("cargo:rerun-if-changed=Entitlements.plist");
    println!("cargo:rerun-if-changed=Info.plist");
    println!("cargo:rerun-if-changed=speech/transcriber.swift");
    println!("cargo:rerun-if-changed=native/maps.swift");

    let target = std::env::var("TARGET").unwrap_or_default();
    // Lets speech.rs / maps.rs locate the triple-suffixed helpers in dev/test runs.
    println!("cargo:rustc-env=MUNINN_TARGET_TRIPLE={target}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // On-device speech-to-text for voice input. The SpeechAnalyzer (macOS 26) path is
        // compile-gated on the installed SDK — older SDKs (e.g. CI runners) still build the
        // SFSpeechRecognizer fallback.
        let speech_flags: &[&str] = if sdk_major() >= 26 { &["-D", "HAVE_SPEECHANALYZER"] } else { &[] };
        compile_helper(&target, "muninn-transcriber", "speech/transcriber.swift", speech_flags);
        // Apple Maps (MapKit) for the map tools — no API key.
        compile_helper(&target, "muninn-maps", "native/maps.swift", &[]);
    }
    tauri_build::build();
}
