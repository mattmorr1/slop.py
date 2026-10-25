//! The Claude Code plugin under `plugin/`: its manifests agree with the crate, and the
//! `slop-run` launcher resolves, caches and fails the way the PRD says.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn json(rel: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(root().join(rel)).unwrap()).unwrap()
}

#[test]
fn manifests_agree_with_the_crate() {
    let plugin = json("plugin/.claude-plugin/plugin.json");
    assert_eq!(plugin["version"], VERSION);
    assert_eq!(plugin["mcpServers"]["slop"]["args"], serde_json::json!(["mcp"]));
    let market = json(".claude-plugin/marketplace.json");
    assert_eq!((market["name"].as_str(), market["plugins"][0]["name"].as_str(), market["plugins"][0]["source"].as_str()), (Some("slop"), Some("slop"), Some("./plugin")));
    let launcher = std::fs::read_to_string(root().join("plugin/bin/slop-run")).unwrap();
    assert!(launcher.contains(&format!("VERSION={VERSION}\n")));
    let hooks = json("plugin/hooks/hooks.json");
    let events: Vec<&str> = hooks["hooks"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(events, ["PreToolUse", "SessionStart", "Stop"]);
    assert_eq!(hooks["hooks"]["PreToolUse"][0]["matcher"], "Write|Edit|MultiEdit");
    assert!(hooks["hooks"].as_object().unwrap().values().all(|entries| entries[0]["hooks"][0]["timeout"].is_u64()));
}

fn launch(tmp: &Path, path: &str, slop_bin: Option<&str>) -> Output {
    let mut command = Command::new(root().join("plugin/bin/slop-run"));
    command.arg("--version").env_clear().env("TMPDIR", tmp).env("PATH", path);
    if let Some(bin) = slop_bin {
        command.env("SLOP_BIN", bin);
    }
    command.output().unwrap()
}

#[test]
fn launcher_prefers_slop_bin_then_caches_a_matching_path_binary_and_names_slop_bin_on_failure() {
    let tmp = std::env::temp_dir().join(format!("slop-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("bin")).unwrap();
    let exe = env!("CARGO_BIN_EXE_slop");
    let cache = tmp.join(format!("slop-plugin-{VERSION}.path"));
    let expected = format!("slop {VERSION}\n");

    let out = launch(&tmp, "/usr/bin:/bin", Some(exe));
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    assert!(!cache.exists(), "SLOP_BIN is not cached");

    let out = launch(&tmp, "/usr/bin:/bin", None);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SLOP_BIN"));

    std::os::unix::fs::symlink(exe, tmp.join("bin/slop")).unwrap();
    let out = launch(&tmp, &format!("{}:/usr/bin:/bin", tmp.join("bin").display()), None);
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
    assert_eq!(std::fs::read_to_string(&cache).unwrap().trim(), tmp.join("bin/slop").to_string_lossy());
    let out = launch(&tmp, "/usr/bin:/bin", None);
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "a cached path needs no PATH lookup");
    let _ = std::fs::remove_dir_all(&tmp);
}
