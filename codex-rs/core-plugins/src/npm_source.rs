use codex_utils_absolute_path::AbsolutePathBuf;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

const NPM_PLUGIN_SOURCE_STAGING_DIR: &str = "plugins/.marketplace-plugin-source-staging";

pub(crate) fn materialize_npm_plugin_source(
    codex_home: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
) -> Result<(AbsolutePathBuf, TempDir), String> {
    materialize_npm_plugin_source_with_command(
        codex_home,
        package,
        version,
        registry,
        OsStr::new(npm_command()),
    )
}

fn materialize_npm_plugin_source_with_command(
    codex_home: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    npm_command: &OsStr,
) -> Result<(AbsolutePathBuf, TempDir), String> {
    let staging_root = codex_home.join(NPM_PLUGIN_SOURCE_STAGING_DIR);
    fs::create_dir_all(&staging_root).map_err(|err| {
        format!(
            "failed to create marketplace plugin source staging directory {}: {err}",
            staging_root.display()
        )
    })?;
    let tempdir = tempfile::Builder::new()
        .prefix("marketplace-plugin-source-")
        .tempdir_in(&staging_root)
        .map_err(|err| {
            format!(
                "failed to create marketplace plugin source staging directory in {}: {err}",
                staging_root.display()
            )
        })?;

    install_npm_package(tempdir.path(), package, version, registry, npm_command)?;
    let plugin_root = installed_npm_package_path(tempdir.path(), package);
    if !plugin_root.is_dir() {
        return Err(format!(
            "npm install completed without creating plugin package directory {}",
            plugin_root.display()
        ));
    }
    let plugin_root = AbsolutePathBuf::try_from(plugin_root)
        .map_err(|err| format!("failed to resolve materialized plugin source path: {err}"))?;
    Ok((plugin_root, tempdir))
}

fn install_npm_package(
    destination: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    npm_command: &OsStr,
) -> Result<(), String> {
    let package_spec = version.map_or_else(
        || package.to_string(),
        |version| format!("{package}@{version}"),
    );
    let mut command = Command::new(npm_command);
    command
        .arg("install")
        .arg("--ignore-scripts")
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--no-package-lock")
        .arg("--prefix")
        .arg(destination);
    if let Some(registry) = registry {
        command.arg("--registry").arg(registry);
    }
    command.arg("--").arg(package_spec);

    let output = command
        .output()
        .map_err(|err| format!("failed to run npm install: {err}"))?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "npm install failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn installed_npm_package_path(destination: &Path, package: &str) -> PathBuf {
    let mut path = destination.join("node_modules");
    for segment in package.split('/') {
        path.push(segment);
    }
    path
}

#[cfg(windows)]
fn npm_command() -> &'static str {
    "npm.cmd"
}

#[cfg(not(windows))]
fn npm_command() -> &'static str {
    "npm"
}

#[cfg(test)]
#[path = "npm_source_tests.rs"]
mod tests;
