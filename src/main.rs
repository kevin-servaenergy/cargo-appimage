use anyhow::{anyhow, bail, Context, Result};
use cargo_toml::Value;
use fs_extra::dir::CopyOptions;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const CARGO_APPIMAGE_PACKAGE_PATH: &str = "CARGO_APPIMAGE_PACKAGE_PATH";
const CARGO_APPIMAGE_PACKAGE: &str = "CARGO_APPIMAGE_PACKAGE";
const CARGO_FNAME: &str = "Cargo.toml";
const APPIMAGE_RUNNER: &str = "cargo-appimage-runner";

/// Return path to a package manifest and it's manifest
fn get_manifest() -> Result<(PathBuf, cargo_toml::Manifest)> {
    let package_path = if let Ok(env_package) = std::env::var(CARGO_APPIMAGE_PACKAGE_PATH) {
        PathBuf::from(env_package)
    } else {
        let package_name = std::env::var(CARGO_APPIMAGE_PACKAGE).unwrap_or_default();
        std::env::current_dir()
            .context("Could not get current dir")?
            .join(package_name)
    };

    get_manifest_from_path(package_path)
}

/// Return path to a package manifest and it's manifest from path.
///
/// The path can either be a directory or the path to manifest
fn get_manifest_from_path<P: AsRef<Path>>(
    package_path: P,
) -> Result<(PathBuf, cargo_toml::Manifest)> {
    let package_path = if package_path.as_ref().is_dir() {
        package_path.as_ref().join(CARGO_FNAME)
    } else {
        package_path.as_ref().to_path_buf()
    };
    let manifest = cargo_toml::Manifest::from_path(&package_path).context(format!(
        "Could not load manifest from path: {package_path:?}"
    ))?;
    Ok((package_path, manifest))
}

/// Get the app runner binary installed by Cargo.
fn get_app_runner_binary_path() -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var("HOME").context("Could not get home path")?)
        .join(std::env::var("CARGO_HOME").unwrap_or_else(|_| ".cargo".to_string()))
        .join("bin")
        .join(APPIMAGE_RUNNER);
    if !path.is_file() {
        eprintln!("Warning: Could not get appimage runner from install dir");
        Err(anyhow!("Could not get appimage runner from install dir"))
    } else {
        Ok(path)
    }
}

fn stage_libs<P: AsRef<Path>>(
    lib_dir_staged: P,
    target_prefix: P,
    target: &str,
    name: &str,
) -> Result<Vec<PathBuf>> {
    let lib_dir_staged = lib_dir_staged.as_ref();
    if !lib_dir_staged.exists() {
        std::fs::create_dir(lib_dir_staged).context("Could not create libs directory")?;
    }
    let awk = std::process::Command::new("awk")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .arg("NF == 4 {print $3}; NF == 2 {print $1}")
        .spawn()
        .context("Could not start awk")?;

    awk.stdin
        .context("Make sure you have awk on your system")?
        .write_all(
            &std::process::Command::new("ldd")
                .arg(format!(
                    "{}/{}/{}",
                    target_prefix.as_ref().display(),
                    target,
                    name
                ))
                .output()
                .with_context(|| {
                    format!(
                        "Failed to run ldd on {}/{}/{}",
                        target_prefix.as_ref().display(),
                        target,
                        name
                    )
                })?
                .stdout,
        )?;

    let mut linkedlibs = String::new();
    awk.stdout
        .context("Unknown error ocurred while running awk")?
        .read_to_string(&mut linkedlibs)?;

    fs_extra::dir::create(lib_dir_staged, true).context("Failed to create libs dir")?;

    let mut libs = vec![];
    for line in linkedlibs.lines() {
        let lib_path = lib_dir_staged.join(&line[1..]);
        if line.starts_with('/') && !lib_path.exists() {
            let staged_path = lib_dir_staged.join(
                std::path::Path::new(line)
                    .file_name()
                    .with_context(|| format!("No filename for {}", line))?,
            );
            std::os::unix::fs::symlink(line, &staged_path)
                .with_context(|| format!("Error symlinking {} to {}", line, lib_path.display()))?;
            libs.push(staged_path);
        }
    }
    Ok(libs)
}

fn main() -> Result<()> {
    let (path, meta) = get_manifest()?;
    let path = path.canonicalize().context("Could not canonicalize path")?;
    println!("Found manifest: {path:?}");
    let parent = path.parent().context("Package path has no parent")?;
    println!("Moving into package root: {parent:?}");
    std::env::set_current_dir(parent).context("Could not chdir to package root")?;
    let pkg = meta
        .package
        .context(format!("Cannot load metadata from {CARGO_FNAME}"))?;

    // Create and execute cargo build command.
    let mut command = Command::new("cargo");
    command.arg("build");
    if !std::env::args()
        .skip(2)
        .any(|arg| arg.starts_with("--profile="))
    {
        command.arg("--release");
    }
    command.args(std::env::args().skip(2));
    let status = command.status().context("Failed to build package")?;
    if !status.success() {
        bail!("Failed to build package");
    }

    let cargo_metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .context("Failed to execute cargo metadata")?;
    let target_prefix = cargo_metadata.target_directory;
    let target_stage_dir = PathBuf::from(target_prefix.clone()).join("appimage_build");
    fs_extra::dir::create_all(&target_stage_dir, true)
        .with_context(|| format!("Error creating {}", target_stage_dir.display()))?;

    let assets;
    let target = {
        let profile = std::env::args()
            .skip(2)
            .find(|arg| arg.starts_with("--profile="))
            .map(|arg| arg.split_at(10).1.to_string())
            .unwrap_or_else(|| "release".into());
        std::env::args()
            .skip(2)
            .find(|arg| arg.starts_with("--target="))
            .map(|arg| format!("{}/{}", arg.split_at(9).1, profile))
            .unwrap_or_else(|| profile)
    };
    let link_deps;
    let mut link_exclude_list = Vec::with_capacity(0);
    let mut args = vec![];

    if let Some(meta) = pkg.metadata.as_ref() {
        match meta {
            Value::Table(t) => match t.get("appimage") {
                Some(Value::Table(t)) => {
                    match t.get("assets") {
                        Some(Value::Array(v)) => {
                            assets = v
                                .iter()
                                .filter_map(|v| match v {
                                    Value::String(s) => Some(s),
                                    _ => None,
                                })
                                .collect()
                        }
                        _ => assets = Vec::with_capacity(0),
                    }
                    match t.get("auto_link") {
                        Some(Value::Boolean(v)) => link_deps = v.to_owned(),
                        _ => link_deps = false,
                    }
                    if let Some(Value::Array(v)) = t.get("args") {
                        args = v
                            .iter()
                            .filter_map(|v| match v {
                                Value::String(s) => Some(s),
                                _ => None,
                            })
                            .collect()
                    }
                    if let Some(Value::Array(arr)) = t.get("auto_link_exclude_list") {
                        for v in arr.iter() {
                            if let Value::String(s) = v {
                                link_exclude_list.push(glob::Pattern::new(s).context(
                                    "Auto-link exclude list item not a valid glob pattern",
                                )?);
                            }
                        }
                    }
                }
                _ => {
                    assets = Vec::with_capacity(0);
                    link_deps = false
                }
            },
            _ => {
                assets = Vec::with_capacity(0);
                link_deps = false
            }
        };
    } else {
        assets = Vec::with_capacity(0);
        link_deps = false;
    }

    for currentbin in meta.bin {
        let name = currentbin.name.unwrap_or(pkg.name.clone());
        let appdirpath = std::path::Path::new(&target_prefix).join(name.clone() + ".AppDir");
        fs_extra::dir::create_all(appdirpath.join("usr"), true)
            .with_context(|| format!("Error creating {}", appdirpath.join("usr").display()))?;

        fs_extra::dir::create_all(appdirpath.join("usr/bin"), true)
            .with_context(|| format!("Error creating {}", appdirpath.join("usr/bin").display()))?;

        let lib_dir_staged = appdirpath.join("libs");
        if link_deps {
            stage_libs(
                &lib_dir_staged,
                &PathBuf::from(&target_prefix),
                &target,
                &name,
            )
            .context("Could not stage libs")?;
        }

        if lib_dir_staged.exists() {
            for i in std::fs::read_dir(&lib_dir_staged).context("Could not read libs dir")? {
                let path = &i?.path();

                // Skip if it matches the exclude list.
                if let Some(file_name) = path.file_name().and_then(|p| p.to_str()) {
                    if link_exclude_list.iter().any(|p| p.matches(file_name)) {
                        continue;
                    }
                }

                let link = std::fs::read_link(path)
                    .with_context(|| format!("Error reading link in libs {}", path.display()))?;

                fs_extra::dir::create_all(
                    appdirpath.join(
                        &link
                            .parent()
                            .with_context(|| format!("Lib {} has no parent dir", &link.display()))?
                            .to_str()
                            .with_context(|| format!("{} is not valid Unicode", link.display()))?
                            [1..],
                    ),
                    false,
                )?;
                let dest = appdirpath.join(
                    &link
                        .to_str()
                        .with_context(|| format!("{} is not valid Unicode", link.display()))?[1..],
                );
                std::fs::copy(&link, &dest).with_context(|| {
                    format!("Error copying {} to {}", &link.display(), dest.display())
                })?;
            }
        }

        std::fs::copy(
            format!("{}/{}/{}", target_prefix, &target, &name),
            appdirpath.join(format!("usr/bin/{}", &name)),
        )
        .with_context(|| {
            format!(
                "Cannot find binary file at {}/{}/{}",
                target_prefix, &target, &name
            )
        })?;

        let icon_path = std::path::Path::new("./icon.png");
        let icon_dest_path = appdirpath.join(icon_path.file_name().unwrap());
        if icon_path.is_file() {
            std::fs::copy(icon_path, &icon_dest_path)
                .context(format!("Cannot copy {icon_path:?}"))?;
        } else {
            std::fs::write(&icon_dest_path, [])
                .context(format!("Failed to generate {icon_dest_path:?}"))?;
        }
        fs_extra::copy_items(
            &assets,
            appdirpath.as_path(),
            &CopyOptions {
                overwrite: true,
                buffer_size: 0,
                copy_inside: true,
                ..Default::default()
            },
        )
        .context("Error copying assets")?;
        std::fs::write(
            appdirpath.join("cargo-appimage.desktop"),
            format!(
                "[Desktop Entry]\nName={}\nExec={}\nIcon=icon\nType=Application\nCategories=Utility;", name
                , name),
                )
            .with_context(|| {
                format!(
                    "Error writing desktop file {}",
                    appdirpath.join("cargo-appimage.desktop").display()
                    )
            })?;
        let app_runner_path = get_app_runner_binary_path()?;
        std::fs::copy(&app_runner_path, appdirpath.join("AppRun")).with_context(|| {
            format!(
                "Error copying {} to {}",
                app_runner_path.display(),
                appdirpath.join("AppRun").display()
            )
        })?;

        let mut bin_args = args.to_vec();
        let appdirpath = appdirpath.into_os_string().into_string().unwrap();
        bin_args.push(&appdirpath);

        std::fs::create_dir_all(format!("{}/appimage", &target_prefix))
            .context("Unable to create output dir")?;
        Command::new("appimagetool")
            .args(bin_args)
            .arg(format!("{}/appimage/{}.AppImage", &target_prefix, &name))
            .env("ARCH", platforms::target::TARGET_ARCH.as_str())
            .env("VERSION", pkg.version())
            .status()
            .context("Error occurred: make sure that appimagetool is installed")?;
    }

    Ok(())
}
