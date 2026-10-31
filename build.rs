use std::{
    collections::HashSet,
    env, fs, io,
    path::{Path, PathBuf},
};

const DEFAULT_XKB_ROOT: &str = "/usr/share/X11/xkb";
const XKB_LICENSE: &str = "licenses/xkeyboard-config-COPYING";

fn main() {
    println!("cargo:rerun-if-env-changed=XKB_CONFIG_ROOT");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let generated = out_dir.join("xkb_data.rs");
    let mut source = String::from("pub static XKB_FILES: &[(&str, &[u8])] = &[\n");
    let root = env::var_os("XKB_CONFIG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_XKB_ROOT));
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));

    if root.is_dir() {
        println!("cargo:rerun-if-changed={}", root.display());
        if let Err(error) = bundle_xkb(&root, &out_dir, &manifest_dir, &mut source) {
            panic!("could not bundle XKB data from {}: {error}", root.display());
        }
    } else {
        println!(
            "cargo:warning=XKB data not found at {}; static builds will use the US fallback",
            root.display()
        );
    }
    source.push_str("];\n");
    fs::write(generated, source).expect("write generated XKB data index");
}

fn bundle_xkb(
    root: &Path,
    out_dir: &Path,
    manifest_dir: &Path,
    source: &mut String,
) -> io::Result<()> {
    let root = root.canonicalize()?;
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    collect_files(&root, &root, &mut visited, &mut files)?;
    files.sort();

    for (relative, source_path) in files {
        let output_path = out_dir.join("xkb_data").join(&relative);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source_path, &output_path)?;
        println!("cargo:rerun-if-changed={}", source_path.display());
        let relative = relative.to_string_lossy();
        source.push_str(&format!(
            "({relative:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/xkb_data/{relative}\"))),\n"
        ));
    }
    let license_source = manifest_dir.join(XKB_LICENSE);
    let license_relative = Path::new(XKB_LICENSE);
    let license_output = out_dir.join("xkb_data").join(license_relative);
    fs::create_dir_all(license_output.parent().expect("license has a parent"))?;
    fs::copy(&license_source, &license_output)?;
    println!("cargo:rerun-if-changed={}", license_source.display());
    source.push_str(&format!(
        "({XKB_LICENSE:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/xkb_data/{XKB_LICENSE}\"))),\n"
    ));
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    visited: &mut HashSet<PathBuf>,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    let canonical = directory.canonicalize()?;
    if !canonical.starts_with(root) || !visited.insert(canonical.clone()) {
        return Ok(());
    }
    for entry in fs::read_dir(&canonical)? {
        let entry = entry?;
        let path = entry.path().canonicalize()?;
        if !path.starts_with(root) {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, visited, files)?;
        } else if path.is_file() {
            files.push((
                path.strip_prefix(root)
                    .map_err(io::Error::other)?
                    .to_owned(),
                path,
            ));
        }
    }
    Ok(())
}
