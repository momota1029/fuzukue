use std::io;
use std::process::Command;

pub(crate) fn tools(names: &[&str]) -> io::Result<bool> {
    for name in names {
        match Command::new(name).arg("--version").output() {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(skip(&format!("missing executable {name}")));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

pub(crate) fn has_file(name: &str, options: &[&str]) -> io::Result<bool> {
    let output = Command::new("kpsewhich").args(options).arg(name).output()?;
    match output.status.code() {
        Some(0) if !output.stdout.is_empty() => Ok(true),
        Some(1) if output.stdout.is_empty() => Ok(skip(&format!("missing TeX file {name}"))),
        _ => Err(io::Error::other(format!(
            "kpsewhich {name} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

pub(crate) fn ready(format: &str, extra_tools: &[&str]) -> io::Result<bool> {
    Ok(tools(&[format, "kpsewhich"])?
        && tools(extra_tools)?
        && has_file(&format!("{format}.fmt"), &["-engine=/", "-format=fmt"])?)
}

fn skip(reason: &str) -> bool {
    let thread = std::thread::current();
    eprintln!("SKIPPED: {}: {reason}", thread.name().unwrap_or("TeX test"));
    false
}
