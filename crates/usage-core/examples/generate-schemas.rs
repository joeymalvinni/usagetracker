use std::{env, fs, path::PathBuf};

use usage_core::{request_schema_v3, response_schema_v3, API_VERSION};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("docs/api/schemas/v3"));
    fs::create_dir_all(&output)?;

    write_schema(output.join("request.json"), &request_schema_v3())?;
    write_schema(output.join("response.json"), &response_schema_v3())?;
    eprintln!(
        "generated protocol v{API_VERSION} schemas in {}",
        output.display()
    );
    Ok(())
}

fn write_schema(
    path: PathBuf,
    schema: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut json = serde_json::to_vec_pretty(schema)?;
    json.push(b'\n');
    fs::write(path, json)?;
    Ok(())
}
