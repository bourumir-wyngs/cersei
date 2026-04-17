use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn portable_home_dir() -> Option<PathBuf> {
    dirs::home_dir().or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                let mut home = PathBuf::from(drive);
                home.push(path);
                Some(home)
            })
    })
}

fn whitelist_arg(path: &Path) -> OsString {
    let mut arg = OsString::from("--whitelist=");
    arg.push(path);
    arg
}

fn home_entries_and_workspace_firejail_args_from_home(
    workspace_root: &Path,
    home_dir: Option<&Path>,
    home_entries: &[&str],
) -> Vec<OsString> {
    let mut args = Vec::with_capacity(home_entries.len() + 1);
    args.push(whitelist_arg(workspace_root));

    if let Some(home_dir) = home_dir {
        for entry in home_entries {
            args.push(whitelist_arg(&home_dir.join(entry)));
        }
    }

    args
}

pub fn home_entries_and_workspace_firejail_args(
    workspace_root: &Path,
    home_entries: &[&str],
) -> Vec<OsString> {
    home_entries_and_workspace_firejail_args_from_home(
        workspace_root,
        portable_home_dir().as_deref(),
        home_entries,
    )
}

pub fn resolve_directory_in_workspace(
    base_cwd: &Path,
    requested_dir: Option<&str>,
    workspace_root: &Path,
    tool_name: &str,
) -> std::result::Result<(PathBuf, PathBuf), String> {
    let canonical_root = workspace_root
        .canonicalize()
        .map_err(|e| format!("Cannot resolve working root: {}", e))?;
    let canonical_base = base_cwd
        .canonicalize()
        .map_err(|e| format!("Cannot resolve current {} directory: {}", tool_name, e))?;

    if !canonical_base.starts_with(&canonical_root) {
        return Err(format!(
            "Current {} directory '{}' is outside the allowed root '{}'",
            tool_name,
            canonical_base.display(),
            canonical_root.display()
        ));
    }

    let cwd = if let Some(dir) = requested_dir {
        let candidate = canonical_base.join(dir);
        let canonical_candidate = candidate
            .canonicalize()
            .map_err(|e| format!("Cannot resolve directory '{}': {}", dir, e))?;
        if !canonical_candidate.starts_with(&canonical_root) {
            return Err(format!(
                "Directory '{}' is outside the allowed root '{}'",
                dir,
                canonical_root.display()
            ));
        }
        canonical_candidate
    } else {
        canonical_base
    };

    Ok((cwd, canonical_root))
}

#[cfg(test)]
mod tests {
    use super::{
        home_entries_and_workspace_firejail_args_from_home, resolve_directory_in_workspace,
    };
    use tempfile::tempdir;

    #[test]
    fn firejail_args_whitelist_workspace_and_home_entries() {
        let workspace = tempdir().unwrap();
        let home = tempdir().unwrap();

        let args = home_entries_and_workspace_firejail_args_from_home(
            workspace.path(),
            Some(home.path()),
            &[".cargo", ".rustup"],
        );

        let rendered: Vec<String> = args
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(rendered.len(), 3);
        assert_eq!(
            rendered[0],
            format!("--whitelist={}", workspace.path().display())
        );
        assert_eq!(
            rendered[1],
            format!("--whitelist={}", home.path().join(".cargo").display())
        );
        assert_eq!(
            rendered[2],
            format!("--whitelist={}", home.path().join(".rustup").display())
        );
    }

    #[test]
    fn rejects_current_directory_outside_workspace() {
        let workspace = tempdir().unwrap();
        let outside = tempdir().unwrap();

        let err = resolve_directory_in_workspace(outside.path(), None, workspace.path(), "cargo")
            .unwrap_err();
        assert!(err.contains("outside the allowed root"));
    }
}
