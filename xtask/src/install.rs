//! Source installation owns a copied executable and runtime, independent of the checkout.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::DynError;

const OWNER_FILE: &str = ".mitos-source-install";
const OWNER: &str = "Mitos source installer runtime v1\n";

struct Options {
    root: PathBuf,
    runtime_dir: PathBuf,
    profile: String,
}

#[derive(serde::Deserialize)]
struct CargoConfig {
    install: Option<CargoInstallConfig>,
}

#[derive(serde::Deserialize)]
struct CargoInstallConfig {
    root: Option<String>,
}

fn non_empty_env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn read_cargo_install_root(path: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(path).ok()?;
    let config: CargoConfig = toml::from_str(&contents).ok()?;
    let root = config.install?.root?;
    if root.is_empty() {
        return None;
    }
    Some(stdx::path::expand_tilde(Path::new(&root)).into_owned())
}

fn cargo_config_install_root(cwd: &Path, cargo_home: Option<&Path>) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let dot_cargo = dir.join(".cargo");
        for filename in ["config.toml", "config"] {
            if let Some(root) = read_cargo_install_root(&dot_cargo.join(filename)) {
                return Some(root);
            }
        }
    }
    if let Some(home) = cargo_home {
        for filename in ["config.toml", "config"] {
            if let Some(root) = read_cargo_install_root(&home.join(filename)) {
                return Some(root);
            }
        }
    }
    None
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, DynError> {
        let mut root = None;
        let mut runtime_dir = None;
        let mut profile = "release".to_owned();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--root" => {
                    root = Some(PathBuf::from(args.next().ok_or("--root requires a path")?))
                }
                "--runtime-dir" => {
                    runtime_dir = Some(PathBuf::from(
                        args.next().ok_or("--runtime-dir requires a path")?,
                    ))
                }
                "--profile" => profile = args.next().ok_or("--profile requires a profile name")?,
                _ => return Err(format!("unexpected install argument: {arg}").into()),
            }
        }
        let cwd = env::current_dir()?;
        let cargo_home = non_empty_env_path("CARGO_HOME")
            .or_else(|| stdx::path::home_dir().ok().map(|home| home.join(".cargo")));
        let root = match root
            .or_else(|| non_empty_env_path("CARGO_INSTALL_ROOT"))
            .or_else(|| cargo_config_install_root(&cwd, cargo_home.as_deref()))
            .or(cargo_home)
        {
            Some(root) => stdx::path::expand_tilde(root).into_owned(),
            None => stdx::path::home_dir()?.join(".cargo"),
        };
        let runtime_dir = match runtime_dir {
            Some(dir) => cwd.join(stdx::path::expand_tilde(dir)),
            None => loader::data_dir().join("runtime"),
        };
        Ok(Self {
            root: cwd.join(root),
            runtime_dir,
            profile,
        })
    }
}

pub fn install(args: impl Iterator<Item = String>) -> Result<(), DynError> {
    let options = Options::parse(args)?;
    let bin = options.root.join("bin");
    check_runtime(&options.runtime_dir)?;
    let sources = loader::config::grammar_sources(&workspace::ResolvedWorkspace::current());
    if let Some(diagnostic) = sources.skipped {
        eprintln!("{diagnostic}");
    }
    let grammars = loader::grammar::get_grammar_configs(sources.value?)?;
    let runtime = crate::path::runtime();
    grammar_build::fetch_grammars(grammars.clone(), &runtime, true)?;
    grammar_build::build_grammars(grammars, &runtime, None, true)?;
    let executable = build(&options.profile)?;
    install_files(
        &executable,
        &crate::path::runtime(),
        &bin,
        &options.runtime_dir,
    )?;
    println!("Installed {}", bin.join(executable_name()).display());
    println!("Installed runtime {}", options.runtime_dir.display());
    println!(
        "Make sure {} is on PATH. Run ms --health to check the installation.",
        bin.display()
    );
    Ok(())
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "ms.exe"
    } else {
        "ms"
    }
}

fn build(profile: &str) -> Result<PathBuf, DynError> {
    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(crate::path::project_root())
        .args([
            "build",
            "--locked",
            "--package",
            "term",
            "--bin",
            "ms",
            "--profile",
            profile,
            "--message-format=json-render-diagnostics",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        return Err("Mitos build failed; installation was not changed".into());
    }
    // Use Cargo's artifact path, including custom target directories and profiles.
    String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact" && message["target"]["name"] == "ms")
                .then(|| message["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .ok_or_else(|| "Cargo did not report the ms executable".into())
}

fn check_runtime(runtime: &Path) -> Result<(), DynError> {
    let metadata = match fs::symlink_metadata(runtime) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir()
        && !is_link(&metadata)
        && fs::read_to_string(runtime.join(OWNER_FILE)).ok().as_deref() == Some(OWNER)
    {
        return Ok(());
    }
    Err(format!(
        "{} is not a runtime managed by this installer. Choose another --runtime-dir or move the existing runtime before installing",
        runtime.display()
    ).into())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Junctions and other reparse points must not be treated as owned directories.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

fn copy_runtime(source: &Path, destination: &Path, relative: &Path) -> Result<(), DynError> {
    fs::create_dir_all(destination.join(relative))?;
    for entry in fs::read_dir(source.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if path == Path::new("grammars").join("sources") {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link(&metadata) {
            return Err(format!("runtime asset is a link: {}", entry.path().display()).into());
        } else if metadata.is_dir() {
            copy_runtime(source, destination, &path)?;
        } else if metadata.is_file() {
            fs::copy(entry.path(), destination.join(path))?;
        } else {
            return Err(format!(
                "runtime asset is not a regular file: {}",
                entry.path().display()
            )
            .into());
        }
    }
    Ok(())
}

fn install_files(
    executable: &Path,
    source: &Path,
    bin: &Path,
    runtime: &Path,
) -> Result<(), DynError> {
    check_runtime(runtime)?;
    let runtime_parent = runtime.parent().ok_or_else(|| {
        format!(
            "runtime directory has no parent directory: {}",
            runtime.display()
        )
    })?;
    fs::create_dir_all(bin)?;
    fs::create_dir_all(runtime_parent)?;
    // Stage each target on its own parent filesystem so atomic renames never cross mounts.
    let bin_stage = tempfile::Builder::new()
        .prefix(".mitos-install-bin-")
        .tempdir_in(bin)?;
    let runtime_stage = tempfile::Builder::new()
        .prefix(".mitos-install-runtime-")
        .tempdir_in(runtime_parent)?;
    let staged_runtime = runtime_stage.path().join("runtime");
    copy_runtime(source, &staged_runtime, Path::new(""))?;
    fs::write(staged_runtime.join(OWNER_FILE), OWNER)?;
    let staged_executable = bin_stage.path().join(executable_name());
    fs::copy(executable, &staged_executable)?;

    let installed_executable = bin.join(executable_name());
    let previous_executable = bin_stage.path().join("previous-ms");
    let previous_runtime = runtime_stage.path().join("previous-runtime");
    // Moving the old files aside also works on Windows, where rename cannot
    // replace an existing executable or a populated directory.
    let mut saved_runtime = false;
    let mut saved_executable = false;
    let mut placed_runtime = false;
    let result = (|| -> Result<(), DynError> {
        if runtime.exists() {
            fs::rename(runtime, &previous_runtime)?;
            saved_runtime = true;
        }
        if fs::symlink_metadata(&installed_executable).is_ok() {
            if fs::symlink_metadata(&installed_executable)?.is_dir() {
                return Err(format!("{} is a directory", installed_executable.display()).into());
            }
            fs::rename(&installed_executable, &previous_executable)?;
            saved_executable = true;
        }
        fs::rename(&staged_runtime, runtime)?;
        placed_runtime = true;
        fs::rename(&staged_executable, &installed_executable)?;
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<(), DynError> {
            if placed_runtime {
                fs::remove_dir_all(runtime)?;
            }
            if saved_runtime {
                fs::rename(&previous_runtime, runtime)?;
            }
            if saved_executable {
                fs::rename(&previous_executable, &installed_executable)?;
            }
            Ok(())
        })();
        if let Err(rollback_error) = rollback {
            let bin_backup = bin_stage.keep();
            let runtime_backup = runtime_stage.keep();
            return Err(format!(
                "installation failed: {error}; could not restore previous installation: {rollback_error}; backups retained in {} and {}",
                bin_backup.display(),
                runtime_backup.display(),
            )
            .into());
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: impl AsRef<Path>, contents: &str) {
        fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn install_and_upgrade_copy_assets_and_remove_obsolete_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source/runtime");
        let executable = temp.path().join("source/ms");
        let bin = temp.path().join("install/bin");
        let runtime = temp.path().join("share/mitos/runtime");
        write(&executable, "old executable");
        write(source.join("themes/old.toml"), "old theme");
        write(source.join("queries/test/highlights.scm"), "query");
        write(source.join("grammars/test.dll"), "grammar");
        write(source.join("grammars/sources/test/src/parser.c"), "source");
        install_files(&executable, &source, &bin, &runtime).unwrap();
        assert!(!bin.join("runtime").exists());
        assert!(!runtime.join("grammars/sources").exists());
        assert_eq!(
            fs::read_to_string(runtime.join("grammars/test.dll")).unwrap(),
            "grammar"
        );

        fs::remove_file(source.join("themes/old.toml")).unwrap();
        write(source.join("themes/new.toml"), "new theme");
        write(&executable, "new executable");
        install_files(&executable, &source, &bin, &runtime).unwrap();
        fs::remove_dir_all(temp.path().join("source")).unwrap();
        assert!(!runtime.join("themes/old.toml").exists());
        assert_eq!(
            fs::read_to_string(runtime.join("themes/new.toml")).unwrap(),
            "new theme"
        );
        assert_eq!(
            fs::read_to_string(runtime.join("queries/test/highlights.scm")).unwrap(),
            "query"
        );
        assert_eq!(
            fs::read_to_string(bin.join(executable_name())).unwrap(),
            "new executable"
        );
    }

    #[test]
    fn unowned_runtime_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let runtime = temp.path().join("share/mitos/runtime");
        let source = temp.path().join("source");
        let executable = temp.path().join("ms");
        write(source.join("tutor"), "new");
        write(&executable, "new executable");
        write(runtime.join("themes/custom.toml"), "custom");
        write(bin.join(executable_name()), "old executable");
        assert!(install_files(&executable, &source, &bin, &runtime).is_err());
        assert_eq!(
            fs::read_to_string(runtime.join("themes/custom.toml")).unwrap(),
            "custom"
        );
        assert_eq!(
            fs::read_to_string(bin.join(executable_name())).unwrap(),
            "old executable"
        );
    }

    #[test]
    fn failed_install_restores_the_previous_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let runtime = temp.path().join("share/mitos/runtime");
        let source = temp.path().join("source");
        let executable = temp.path().join("ms");
        write(source.join("tutor"), "new");
        write(&executable, "new executable");
        write(runtime.join(OWNER_FILE), OWNER);
        write(runtime.join("tutor"), "old");
        // A directory at the executable path fails after the runtime was moved aside.
        fs::create_dir_all(bin.join(executable_name())).unwrap();
        assert!(install_files(&executable, &source, &bin, &runtime).is_err());
        assert_eq!(fs::read_to_string(runtime.join("tutor")).unwrap(), "old");
        assert!(bin.join(executable_name()).is_dir());
    }

    #[test]
    fn reads_install_root_from_cargo_config() {
        let temp = tempfile::tempdir().unwrap();
        let cargo_home = temp.path().join("cargo");
        let project = temp.path().join("workspace/sub");
        fs::create_dir_all(&project).unwrap();
        write(
            cargo_home.join("config.toml"),
            "[install]\nroot = \"/opt/local\"\n",
        );
        assert_eq!(
            cargo_config_install_root(&project, Some(&cargo_home)),
            Some(PathBuf::from("/opt/local"))
        );

        // Closer ancestor .cargo/config.toml takes precedence over CARGO_HOME.
        write(
            temp.path().join("workspace/.cargo/config.toml"),
            "[install]\nroot = \"/custom/root\"\n",
        );
        assert_eq!(
            cargo_config_install_root(&project, Some(&cargo_home)),
            Some(PathBuf::from("/custom/root"))
        );
    }
}
