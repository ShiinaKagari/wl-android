use std::path::Path;
use std::process::Command;

fn main() {
    let shader_dir = Path::new("shaders");
    if !shader_dir.exists() {
        return;
    }

    for entry in std::fs::read_dir(shader_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "vert" || e == "frag") {
            let output = path.with_extension("spv");
            let status = try_compile_shader(&path, &output);
            match status {
                Ok(s) if s.success() => {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
                _ => {
                    println!("cargo:warning=shader compilation skipped, using pre-compiled SPIR-V");
                }
            }
        }
    }
}

fn try_compile_shader(path: &Path, output: &Path) -> Result<std::process::ExitStatus, String> {
    if let Ok(s) = Command::new("glslc").arg(path).arg("-o").arg(output).status() {
        return Ok(s);
    }
    if let Ok(s) = Command::new("glslangValidator")
        .arg("-V")
        .arg(path)
        .arg("-o")
        .arg(output)
        .status()
    {
        return Ok(s);
    }
    Err("no shader compiler found".into())
}
