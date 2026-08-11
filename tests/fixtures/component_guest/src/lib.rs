#[cfg(target_arch = "wasm32")]
mod guest {
    use std::net::{TcpStream, UdpSocket};

    use serde_json::{Value, json};
    use vvmux_plugin_sdk::component::{self, Guest, PluginError};

    struct ConformanceGuest;

    impl Guest for ConformanceGuest {
        fn initialize(context_json: Vec<u8>) -> Result<(), PluginError> {
            component::parse_json(&context_json)?;
            component::storage_set("initialize-context", &context_json)?;
            component::log("info", "fixture initialized");
            Ok(())
        }

        fn invoke(
            action: String,
            input_json: Vec<u8>,
            context_json: Vec<u8>,
        ) -> Result<Vec<u8>, PluginError> {
            let input = component::parse_json(&input_json)?;
            let context = component::parse_json(&context_json)?;
            let output = match action.as_str() {
                "echo" => json!({
                    "input": input,
                    "context": context,
                    "initialized": component::storage_get("initialize-context")?.is_some(),
                }),
                "inspect" => component::call("session.inspect", &json!({}))?,
                "storage" => storage_round_trip(input)?,
                "preopens" => probe_preopens(input)?,
                "ambient" => probe_ambient_authority(input),
                "log-flood" => {
                    component::log("warn", &"x".repeat(300 * 1024));
                    json!({"logged": true})
                }
                "trap" => panic!("intentional component trap"),
                "spin" => loop {
                    std::hint::spin_loop();
                },
                _ => return Err(component::error("action_not_found", action)),
            };
            component::log("info", &format!("handled {action}"));
            component::json(&output)
        }

        fn on_event(
            name: String,
            event_json: Vec<u8>,
            context_json: Vec<u8>,
        ) -> Result<(), PluginError> {
            component::parse_json(&event_json)?;
            component::parse_json(&context_json)?;
            component::storage_set("last-event", name.as_bytes())
        }

        fn shutdown() -> Result<(), PluginError> {
            component::storage_set("shutdown", b"called")
        }
    }

    fn storage_round_trip(input: Value) -> Result<Value, PluginError> {
        let key = input
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| component::error("schema_invalid", "key must be a string"))?;
        if let Some(value) = input.get("value").and_then(Value::as_str) {
            component::storage_set(key, value.as_bytes())?;
        }
        let stored = component::storage_get(key)?.unwrap_or_default();
        Ok(json!({"value": String::from_utf8_lossy(&stored)}))
    }

    fn probe_preopens(input: Value) -> Result<Value, PluginError> {
        let package = std::fs::read_to_string("/package/fixture.txt");
        let config = std::fs::read_to_string("/config/config.txt");
        let config_write = std::fs::write("/config/forbidden", b"no");
        let data_write = std::fs::write("/data/allowed", b"yes");
        let data = std::fs::read_to_string("/data/allowed");
        let undeclared = std::fs::read_to_string("/etc/passwd");
        let undeclared_write = std::fs::write("/forbidden", b"no");
        Ok(json!({
            "expected": input,
            "package": package.ok(),
            "config": config.ok(),
            "config_write_denied": config_write.is_err(),
            "data_write_succeeded": data_write.is_ok(),
            "data_write": data.ok(),
            "undeclared_denied": undeclared.is_err(),
            "undeclared_write_denied": undeclared_write.is_err(),
        }))
    }

    fn probe_ambient_authority(input: Value) -> Value {
        let environment = std::env::vars().collect::<Vec<_>>();
        let tcp = input
            .get("tcp")
            .and_then(Value::as_str)
            .unwrap_or("127.0.0.1:9");
        let tcp_denied = TcpStream::connect(tcp).is_err();
        let udp_denied = UdpSocket::bind("127.0.0.1:0").is_err();
        let dns_denied = std::net::ToSocketAddrs::to_socket_addrs(&"localhost:80").is_err();
        let process_denied = std::process::Command::new("true").status().is_err();
        json!({
            "environment": environment,
            "tcp_denied": tcp_denied,
            "udp_denied": udp_denied,
            "dns_denied": dns_denied,
            "process_denied": process_denied,
        })
    }

    component::export!(ConformanceGuest with_types_in component);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn host_placeholder() {}
