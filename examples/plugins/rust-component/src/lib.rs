#[cfg(target_arch = "wasm32")]
mod guest {
    use serde_json::json;
    use vvmux_plugin_sdk::component::{self, Guest, PluginError};

    struct Hello;

    impl Guest for Hello {
        fn initialize(context_json: Vec<u8>) -> Result<(), PluginError> {
            component::parse_json(&context_json)?;
            component::log("info", "reference component initialized");
            Ok(())
        }

        fn invoke(
            action: String,
            input_json: Vec<u8>,
            context_json: Vec<u8>,
        ) -> Result<Vec<u8>, PluginError> {
            if action != "greet" {
                return Err(component::error("action_not_found", action));
            }
            let input = component::parse_json(&input_json)?;
            let context = component::parse_json(&context_json)?;
            let name = input
                .get("name")
                .and_then(|value| value.as_str())
                .ok_or_else(|| component::error("schema_invalid", "name is required"))?;
            component::json(&json!({
                "message": format!("Hello, {name}!"),
                "correlation_id": context.get("correlation_id"),
            }))
        }

        fn on_event(
            _name: String,
            _event_json: Vec<u8>,
            _context_json: Vec<u8>,
        ) -> Result<(), PluginError> {
            Ok(())
        }

        fn shutdown() -> Result<(), PluginError> {
            Ok(())
        }
    }

    component::export!(Hello with_types_in component);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn host_placeholder() {}
