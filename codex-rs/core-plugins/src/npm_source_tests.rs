use super::*;
use pretty_assertions::assert_eq;

#[test]
fn installed_npm_package_path_supports_scoped_packages() {
    assert_eq!(
        installed_npm_package_path(Path::new("/tmp/install"), "@acme/plugin"),
        Path::new("/tmp/install/node_modules/@acme/plugin")
    );
}

#[cfg(unix)]
#[test]
fn materialize_npm_plugin_source_uses_npm_installed_package_root() {
    use std::os::unix::fs::PermissionsExt;

    let codex_home = tempfile::tempdir().expect("create codex home");
    let fake_npm_dir = tempfile::tempdir().expect("create fake npm directory");
    let fake_npm = fake_npm_dir.path().join("npm");
    fs::write(
        &fake_npm,
        r#"#!/bin/sh
prefix=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "--prefix" ]; then
    prefix="$argument"
  fi
  previous="$argument"
done
mkdir -p "$prefix/node_modules/@acme/plugin/.codex-plugin"
printf '%s\n' "$@" > "$prefix/args.txt"
printf '{"name":"plugin"}\n' > "$prefix/node_modules/@acme/plugin/.codex-plugin/plugin.json"
"#,
    )
    .expect("write fake npm");
    let mut permissions = fs::metadata(&fake_npm)
        .expect("read fake npm metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_npm, permissions).expect("make fake npm executable");

    let (plugin_root, tempdir) = materialize_npm_plugin_source_with_command(
        codex_home.path(),
        "@acme/plugin",
        Some("^1.2.0"),
        Some("https://npm.example.com"),
        fake_npm.as_os_str(),
    )
    .expect("materialize npm source");

    assert_eq!(
        plugin_root.as_path(),
        tempdir.path().join("node_modules/@acme/plugin")
    );
    assert!(
        plugin_root
            .as_path()
            .join(".codex-plugin/plugin.json")
            .is_file()
    );
    let args = fs::read_to_string(tempdir.path().join("args.txt")).expect("read npm arguments");
    assert!(args.contains("--ignore-scripts"));
    assert!(args.contains("--registry"));
    assert!(args.contains("https://npm.example.com"));
    assert!(args.contains("@acme/plugin@^1.2.0"));
}
