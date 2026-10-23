// The opener plugin's `allow-open-url` enables the command with no URL scope,
// so every `openUrl` is refused unless the capability granting it adds one.

use serde_json::Value;

#[test]
fn every_capability_that_opens_links_may_open_web_links() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities");
    let mut granting = 0;
    for entry in std::fs::read_dir(dir).expect("list capabilities") {
        let path = entry.expect("capability entry").path();
        let text = std::fs::read_to_string(&path).expect("read a capability");
        let capability: Value = serde_json::from_str(&text).expect("parse a capability");
        for permission in capability["permissions"]
            .as_array()
            .expect("permissions list")
        {
            let granted = permission == "opener:allow-open-url"
                || permission["identifier"] == "opener:allow-open-url";
            if !granted {
                continue;
            }
            granting += 1;
            let scopes: Vec<&str> = permission["allow"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|scope| scope["url"].as_str())
                .collect();
            for wanted in ["https://*", "http://*"] {
                assert!(
                    scopes.contains(&wanted),
                    "{} grants opener:allow-open-url without {wanted}; scopes: {scopes:?}",
                    path.display()
                );
            }
        }
    }
    assert!(granting > 0, "no capability grants opener:allow-open-url");
}
