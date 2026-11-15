use std::{env, fs, path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=moonlight.lock");
    println!("cargo:rerun-if-changed=tools/fetch-moonlight.sh");
    println!("cargo:rerun-if-changed=native/bridge.c");
    assert!(Command::new("sh").arg("tools/fetch-moonlight.sh").status().expect("fetch Moonlight").success(), "fetching the pinned Moonlight sources failed");
    let root = Path::new("vendor/moonlight-common-c");
    let mut build = cc::Build::new();
    build.std("c11").warnings(false).define("NDEBUG", None).define("_GNU_SOURCE", None);
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
        // Zig's Clang separates EVEX512 from AVX512F. Nanors still runtime-gates its AVX512 functions.
        build.flag_if_supported("-mevex512");
    }
    for flag in ["HAS_SOCKLEN_T", "HAS_FCNTL", "HAS_IOCTL", "HAS_POLL", "HAS_GETADDRINFO", "HAS_GETNAMEINFO", "HAS_INET_PTON", "HAS_INET_NTOP", "HAS_MSGHDR_FLAGS"] {
        build.define(flag, "1");
    }
    build.include(root.join("src")).include(root.join("enet/include"))
        .include(root.join("nanors")).include(root.join("nanors/deps")).include(root.join("nanors/deps/obl"))
        .include(env::var("DEP_OPENSSL_INCLUDE").expect("OpenSSL include directory"));
    let mut sources: Vec<_> = fs::read_dir(root.join("src")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "c")).collect();
    sources.sort();
    build.files(sources);
    for source in ["callbacks.c", "compress.c", "host.c", "list.c", "packet.c", "peer.c", "protocol.c", "unix.c"] {
        build.file(root.join("enet").join(source));
    }
    for source in ["rs.c", "deps/obl/oblas_common.c", "deps/obl/oblas_lite.c"] {
        build.file(root.join("nanors").join(source));
    }
    build.file("native/bridge.c").compile("lumagame_moonlight");
    println!("cargo:rustc-link-lib=pthread");
}
