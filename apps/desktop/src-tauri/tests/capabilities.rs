// The opener plugin's `allow-open-url` enables the command with no URL scope,
// so every `openUrl` is refused unless the main window's capability grants one.

use serde_json::Value;

#[test]
fn main_window_may_open_web_links_in_the_browser() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities/default.json");
    let text = std::fs::read_to_string(path).expect("read the default capability");
    let capability: Value = serde_json::from_str(&text).expect("parse the default capability");
    let scopes: Vec<&str> = capability["permissions"]
        .as_array()
        .expect("permissions list")
        .iter()
        .filter(|permission| permission["identifier"] == "opener:allow-open-url")
        .flat_map(|permission| permission["allow"].as_array().into_iter().flatten())
        .filter_map(|scope| scope["url"].as_str())
        .collect();
    for wanted in ["https://*", "http://*"] {
        assert!(
            scopes.contains(&wanted),
            "opener:allow-open-url must allow {wanted}; scopes: {scopes:?}"
        );
    }
}
