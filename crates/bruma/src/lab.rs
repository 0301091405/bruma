//! The test lab: one page with something to try every feature on. It ships
//! inside the executable, so `bruma lab` works anywhere.

use std::path::PathBuf;

const PAGE: &str = include_str!("../../../lab/index.html");

/// Writes the lab page to the temp folder and opens it in the default browser.
pub fn open() -> Result<PathBuf, String> {
    // A packaged install's temporary folder is private to it; the browser
    // could not read the page from there.
    let dir = crate::platform::package_folder()
        .unwrap_or_else(std::env::temp_dir)
        .join("bruma-lab");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("index.html");
    std::fs::write(&path, PAGE).map_err(|e| format!("{}: {e}", path.display()))?;
    std::process::Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("could not open the lab page: {e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_embedded_and_self_contained() {
        assert!(PAGE.contains("<title>Bruma"));
        assert!(
            !PAGE.contains("http://") && !PAGE.contains("https://"),
            "no network resources"
        );
    }
}
