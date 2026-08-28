//! Embed the daemon's `Info.plist` on macOS.
//!
//! macOS does not merely *refuse* a camera or microphone to a process whose
//! responsible executable declares no reason for wanting one — it kills it.
//! The first video call in a shipped build would take the daemon, and the
//! session with it, and the mistake is invisible everywhere else: no other
//! platform asks, and this one only asks at runtime.
//!
//! The release packages both binaries raw rather than in an `.app`, so there
//! is no bundle to put the file in and it goes where an unbundled executable
//! carries it: a `__TEXT,__info_plist` section of the binary itself, which is
//! where the system looks first.
//!
//! On the daemon alone, because the daemon is the process that opens the
//! devices — the same split that makes a call keep working with the window
//! closed.

fn main() {
    println!("cargo:rerun-if-changed=oxidezapd.plist");
    // The *target* OS: a build script runs on the host, and cross-compiling
    // for macOS from anything else must still embed it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        println!("cargo:warning=no manifest directory; oxidezapd will ship without its Info.plist");
        return;
    };
    let plist = std::path::Path::new(&manifest).join("oxidezapd.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
