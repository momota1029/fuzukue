use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../locales");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut directories: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    directories.sort();
    let mut code = String::from("const RESOURCES: &[(&str, &[&str])] = &[\n");
    for directory in directories.into_iter().filter(|path| path.is_dir()) {
        let locale = directory.file_name().unwrap().to_str().unwrap();
        code.push_str(&format!("({locale:?}, &[\n"));
        let mut files: Vec<_> = fs::read_dir(directory.clone())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        files.sort();
        for file in files
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "ftl"))
        {
            code.push_str(&format!("include_str!({file:?}),\n"));
        }
        code.push_str("]),\n");
    }
    code.push_str("];\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("catalogs.rs");
    fs::write(output, code).unwrap();
}
