//! Guarded deletion. Clearing a directory must (1) resolve to a path *strictly
//! inside* a fractal-managed disposable folder — never `/`, `$HOME`, or a
//! shallow path — and (2) be confirmed by the user. Anything else is refused,
//! so a wrong or relative target can never delete unintended files.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The only directories fractal will clear. A target must live under one of
/// these. Mirrors the shell allow-list `"$HOME/fractal-test/"*` etc.
pub(crate) fn disposable_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    match std::env::var_os("FRACTAL_HOME") {
        Some(home) if !home.is_empty() => roots.push(PathBuf::from(home)),
        _ => {
            if let Some(home) = std::env::var_os("HOME") {
                roots.push(PathBuf::from(&home).join(".fractal"));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for name in ["fractal-test", "fractal-demo", "fractal-runs"] {
            roots.push(home.join(name));
        }
    }
    roots
}

/// Resolve to an absolute path (canonicalizing the parent when the target itself
/// does not yet exist), so `..`/symlink tricks cannot escape the allow-list.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    match (
        path.parent().and_then(|p| std::fs::canonicalize(p).ok()),
        path.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// Pure guard: is `resolved` a safe target given the disposable `roots` and
/// `home`? Refuses `/`, `home` itself, shallow paths, and anything outside all
/// roots.
fn check(resolved: &Path, roots: &[PathBuf], home: Option<&Path>) -> Result<()> {
    if resolved == Path::new("/") || home == Some(resolved) || resolved.components().count() < 3 {
        bail!("Refusing unsafe path: {}", resolved.display());
    }
    if roots
        .iter()
        .any(|root| resolved != root && resolved.starts_with(root))
    {
        Ok(())
    } else {
        bail!(
            "Refusing to delete {} — not inside a fractal disposable folder ({})",
            resolved.display(),
            roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Verify `target` is a permitted disposable directory, returning its resolved
/// path or a refusal error.
pub(crate) fn ensure_disposable(target: &Path) -> Result<PathBuf> {
    let resolved = resolve(target);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let roots: Vec<PathBuf> = disposable_roots()
        .iter()
        .map(|root| resolve(root))
        .collect();
    check(&resolved, &roots, home.as_deref())?;
    Ok(resolved)
}

/// Guarded clear of a directory's CONTENTS (not the directory itself), after a
/// `[y/N]` confirmation (skipped when `assume_yes`). Returns how many top-level
/// entries were removed.
pub(crate) fn guarded_clear(target: &Path, assume_yes: bool) -> Result<usize> {
    let resolved = ensure_disposable(target)?;
    if !resolved.is_dir() {
        bail!("{} is not a directory", resolved.display());
    }
    if !assume_yes {
        print!(
            "Delete ALL contents of {}? This cannot be undone. [y/N]: ",
            resolved.display()
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("failed to read confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("Aborted — nothing was deleted.");
        }
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&resolved)
        .with_context(|| format!("failed to read {}", resolved.display()))?
    {
        let path = entry?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/Users/x/.fractal"),
            PathBuf::from("/Users/x/fractal-test"),
            PathBuf::from("/Users/x/fractal-demo"),
        ]
    }

    #[test]
    fn allows_a_folder_inside_a_disposable_root() {
        assert!(check(
            Path::new("/Users/x/fractal-test/reverse-demo"),
            &roots(),
            Some(Path::new("/Users/x")),
        )
        .is_ok());
        assert!(check(
            Path::new("/Users/x/.fractal/graphs"),
            &roots(),
            Some(Path::new("/Users/x"))
        )
        .is_ok());
    }

    #[test]
    fn refuses_outside_the_allow_list() {
        for bad in [
            "/",
            "/Users/x",
            "/Users",
            "/tmp/whatever",
            "/Users/x/projects/app",
        ] {
            assert!(
                check(Path::new(bad), &roots(), Some(Path::new("/Users/x"))).is_err(),
                "should refuse {bad}"
            );
        }
    }

    #[test]
    fn refuses_home_and_root_explicitly() {
        assert!(check(Path::new("/Users/x"), &roots(), Some(Path::new("/Users/x"))).is_err());
        assert!(check(Path::new("/"), &roots(), Some(Path::new("/Users/x"))).is_err());
        for root in roots() {
            assert!(check(&root, &roots(), Some(Path::new("/Users/x"))).is_err());
        }
    }
}
