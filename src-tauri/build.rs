fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads/main");
    let revision = std::env::var("GITHUB_SHA").ok().or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
    });
    println!(
        "cargo:rustc-env=GPT_SWITCH_REVISION={}",
        revision.as_deref().unwrap_or("unknown").trim()
    );
    build_power();
    tauri_build::build()
}

fn build_power() {
    println!("cargo:rerun-if-changed=native/power_client.m");
    println!("cargo:rerun-if-changed=native/power_helper.m");
    println!("cargo:rerun-if-changed=native/power_install.h");
    println!("cargo:rerun-if-changed=native/power_storage_tests.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let helper = out.join("power-helper");
    let object = out.join("power-client.o");
    let run = |program: &str, args: &[&str]| {
        let result = std::process::Command::new(program)
            .args(args)
            .output()
            .expect("native power build tool");
        assert!(
            result.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        result
    };
    let common = [
        "clang",
        "-arch",
        "arm64",
        "-mmacosx-version-min=12.0",
        "-fobjc-arc",
        "-fblocks",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-Wno-unused-parameter",
        "-Wno-incompatible-pointer-types",
    ];
    let mut args = common.to_vec();
    args.extend([
        "native/power_helper.m",
        "-framework",
        "Foundation",
        "-framework",
        "Security",
        "-o",
        helper.to_str().unwrap(),
    ]);
    run("xcrun", &args);
    run(
        "/usr/bin/codesign",
        &[
            "--force",
            "--sign",
            "-",
            "--identifier",
            "com.lich13.gpt-switch.power-helper",
            helper.to_str().unwrap(),
        ],
    );
    let info = run(
        "/usr/bin/codesign",
        &["-dv", "--verbose=4", helper.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&info.stderr);
    let hash = stderr
        .lines()
        .find_map(|l| l.strip_prefix("CDHash="))
        .expect("helper CDHash");
    println!("cargo:rustc-env=GPT_POWER_HELPER={}", helper.display());
    println!("cargo:rustc-env=GPT_POWER_HASH={hash}");
    let mut args = common.to_vec();
    args.extend([
        "-c",
        "native/power_client.m",
        "-o",
        object.to_str().unwrap(),
    ]);
    run("xcrun", &args);
    run(
        "ar",
        &[
            "rcs",
            out.join("libgpt_power.a").to_str().unwrap(),
            object.to_str().unwrap(),
        ],
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=gpt_power");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Security");
    // Expose a safe, standalone simulator for the native helper logic to cargo tests.
    let test = out.join("power-helper-tests");
    let mut args = common.to_vec();
    args.extend([
        "-DPOWER_TEST",
        "-Wno-unused-function",
        "-Wno-unused-variable",
        "native/power_helper.m",
        "-framework",
        "Foundation",
        "-framework",
        "Security",
        "-o",
        test.to_str().unwrap(),
    ]);
    run("xcrun", &args);
    println!("cargo:rustc-env=GPT_POWER_TEST={}", test.display());
}
