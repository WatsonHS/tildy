//! `tildy schema [--json]`: print the embedded config JSON Schema
//! (`schema/config.v1.schema.json`), the field reference for agents. Owner:
//! M2 task A.
use clap::Args;
use serde::Serialize;
use serde_json::Value;
use tildy::config::SCHEMA_V1_JSON;
use tildy::model::{CliEnvelope, ErrorCode, SCHEMA_VERSION};

#[derive(Args)]
pub struct SchemaArgs {
    /// Wrap the schema in the JSON result envelope (`data.schema`).
    #[arg(long)]
    pub json: bool,
    /// Schema for repository display overrides (.tildy.json).
    #[arg(long)]
    pub project: bool,
}

#[derive(Serialize)]
struct SchemaView {
    /// The config `schema_version` this schema describes.
    schema_version: u32,
    schema: Value,
}

pub fn run(a: SchemaArgs) -> i32 {
    let project_schema;
    let schema_json = if a.project {
        project_schema = serde_json::to_string_pretty(&tildy::config::project::schema()).expect("schema serializes");
        project_schema.as_str()
    } else { SCHEMA_V1_JSON };
    if !a.json {
        print!("{schema_json}");
        if !schema_json.ends_with('\n') {
            println!();
        }
        return 0;
    }
    let (env, code) = match serde_json::from_str::<Value>(schema_json) {
        Ok(schema) => (serde_json::to_string_pretty(&CliEnvelope::ok(SchemaView { schema_version: SCHEMA_VERSION, schema })), 0),
        Err(e) => {
            let code = ErrorCode::InternalError;
            (serde_json::to_string_pretty(&CliEnvelope::<()>::err(code, format!("embedded schema is not JSON: {e}"), None)), code.exit_code())
        }
    };
    match env {
        Ok(s) => {
            println!("{s}");
            code
        }
        Err(_) => ErrorCode::InternalError.exit_code(),
    }
}
