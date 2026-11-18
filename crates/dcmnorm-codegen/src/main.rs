//! `cargo run -p dcmnorm-codegen` regenerates the bindings' DICOM constants (see the library
//! docs); `-- --check` only reports outputs that are out of date, exiting 1 if there are any.

fn main() {
    let root = dcmnorm_codegen::repo_root();
    let check = std::env::args().skip(1).any(|arg| arg == "--check");
    if check {
        let stale = dcmnorm_codegen::stale_outputs(&root);
        if stale.is_empty() {
            println!("generated binding constants are up to date");
            return;
        }
        for path in &stale {
            eprintln!("out of date: {path}");
        }
        eprintln!("run `cargo run -p dcmnorm-codegen` and commit the result");
        std::process::exit(1);
    }
    match dcmnorm_codegen::write_outputs(&root) {
        Ok(changed) if changed.is_empty() => println!("generated binding constants are up to date"),
        Ok(changed) => changed.iter().for_each(|path| println!("wrote {path}")),
        Err(error) => {
            eprintln!("dcmnorm-codegen: {error}");
            std::process::exit(1);
        }
    }
}
