//! The Sessions remote (multi-surface U1 scaffold; U3–U5 fill it in).
//!
//! Implements the remote contract from `qq_ui_common::remote`: `mount`
//! parses the config the shell passes, renders into the given root, and
//! `unmount` tears the Leptos tree down. Standalone (`trunk serve` in this
//! directory) it mounts into `#qq-sessions` with an empty server list so the
//! remote can be developed without the shell.

#![forbid(unsafe_code)]

use std::{any::Any, cell::RefCell};

use leptos::{mount::mount_to, prelude::*};
use qq_ui_common::remote::{RemoteConfig, RemoteServer};
use wasm_bindgen::prelude::*;

thread_local! {
    // Leptos's `UnmountHandle<M>` names the view's state type, which is
    // private to the `view!` expansion; dropping it unmounts, so the slot only
    // needs to own it.
    static MOUNTED: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

#[wasm_bindgen]
pub fn mount(root: web_sys::HtmlElement, config: &str) -> Result<(), JsValue> {
    let config = match RemoteConfig::parse(config) {
        Ok(config) => config,
        Err(error) => return Err(JsValue::from_str(&error.to_string())),
    };
    unmount();
    let handle = mount_to(root, move || {
        view! { <Sessions servers=config.servers.clone() /> }
    });
    MOUNTED.with(|slot| *slot.borrow_mut() = Some(Box::new(handle)));
    Ok(())
}

#[wasm_bindgen]
pub fn unmount() {
    MOUNTED.with(|slot| slot.borrow_mut().take());
}

fn main() {
    let root = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("qq-sessions"));
    let Some(root) = root else {
        return;
    };
    if let Err(error) = mount(
        root.unchecked_into(),
        &RemoteConfig::new(Vec::new()).to_json(),
    ) {
        web_sys::console::error_1(&error);
    }
}

#[component]
fn Sessions(servers: Vec<RemoteServer>) -> impl IntoView {
    let empty = servers.is_empty();
    view! {
        <div class="sessions-remote">
            <header class="remote-bar">
                <h1>"Sessions"</h1>
                <span class="count">{format!("{} server{}", servers.len(), if servers.len() == 1 { "" } else { "s" })}</span>
            </header>
            <ul class="bound-servers" class:hidden=empty>
                {servers
                    .iter()
                    .map(|server| {
                        view! {
                            <li>
                                <span class="dot live"></span>
                                <span class="server-name">{server.display_name.clone()}</span>
                                <span class="server-meta">{server.base_url.clone()}</span>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
            <p class="empty" class:hidden=!empty>
                "No servers bound. Connect one in the shell and this remote receives it through "
                <code>"mount(root, config)"</code> "."
            </p>
            <p class="note">
                "Workspaces and the session tree (U3), the transcript (U4), and the composer (U5) render here."
            </p>
        </div>
    }
}
