use std::io;

use clap::Subcommand;
use schemars::JsonSchema;
use serde_json::Value;

/// The two JSON-compatible automation records carried inside typed VVMX messages.
#[derive(JsonSchema)]
#[schemars(untagged)]
#[allow(dead_code)]
enum AutomationRecord {
    Request(crate::ipc::AutomationRequest),
    Response(crate::ipc::AutomationResponse),
}

#[derive(Debug, Subcommand)]
pub(crate) enum ApiCommand {
    /// Print the VVMX automation request/response JSON Schema.
    Schema {
        /// Emit JSON. Reserved so future human-oriented formats remain additive.
        #[arg(long, default_value_t = true)]
        json: bool,
    },
}

pub(crate) fn run(command: ApiCommand) -> io::Result<()> {
    match command {
        ApiCommand::Schema { json: true } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&schema()).map_err(io::Error::other)?
            );
            Ok(())
        }
        ApiCommand::Schema { json: false } => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only --json schema output is currently supported",
        )),
    }
}

pub(crate) fn schema() -> Value {
    let mut value = serde_json::to_value(schemars::schema_for!(AutomationRecord))
        .expect("schema generation is serializable");
    let object = value
        .as_object_mut()
        .expect("schemars produces an object document");
    object.insert(
        "$id".into(),
        Value::String(format!(
            "https://vivido.dev/schemas/vvmux/automation-v{}.json",
            crate::ipc::VERSION
        )),
    );
    object.insert("title".into(), Value::String("vvmux automation API".into()));
    object.insert(
        "description".into(),
        Value::String(
            "Stable JSON projection of the CBOR VVMX automation request and response records."
                .into(),
        ),
    );
    object.insert("x-vvmx-version".into(), Value::from(crate::ipc::VERSION));
    value
}

#[cfg(test)]
mod tests {
    #[test]
    fn schema_is_deterministic_and_carries_the_live_protocol_version() {
        let first = super::schema();
        assert_eq!(first, super::schema());
        assert_eq!(first["x-vvmx-version"], crate::ipc::VERSION);
        assert_eq!(
            first["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        let encoded = serde_json::to_string(&first).unwrap();
        assert!(encoded.contains("agent_prompt"));
        assert!(encoded.contains("timeout_ms"));
        assert!(encoded.contains("AutomationResponse"));
    }
}
