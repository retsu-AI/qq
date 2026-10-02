//! The QQ web shell (ADR-0017, multi-surface U1).
//!
//! One Leptos app that owns the chrome, the connected servers, and an outlet
//! it mounts exactly one remote into at a time. Remotes come from
//! `remotes.json`; the active one is the URL hash (`#/sessions`). Servers are
//! connected through `qq_ui_common::probe`, which refuses an incompatible
//! protocol with a message the person can act on, and live in memory for
//! this tab (the W3 `ServerSet` and IndexedDB profiles arrive with U2).

#![forbid(unsafe_code)]

mod loader;

use std::collections::HashMap;

use leptos::{ev, html, prelude::*, task::spawn_local};
use qq_protocol::ServerConnection;
use qq_ui_common::{
    compat::SUPPORTED_PROTOCOL,
    probe::probe,
    remote::{RemoteConfig, RemoteManifest, RemoteServer},
};

use crate::loader::LoadedRemote;

/// Most servers the shell keeps connected in one tab; the W3 bound.
const MAX_SERVERS: usize = 16;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(Shell);
}

#[derive(Clone)]
enum Manifest {
    Loading,
    Ready(RemoteManifest),
    Failed(String),
}

#[component]
fn Shell() -> impl IntoView {
    let manifest = RwSignal::new_local(Manifest::Loading);
    let servers: RwSignal<Vec<ServerConnection>, LocalStorage> = RwSignal::new_local(Vec::new());
    let active = RwSignal::new(active_remote_from_hash());
    window_event_listener(ev::hashchange, move |_| {
        active.set(active_remote_from_hash())
    });

    spawn_local(async move {
        let loaded = match fetch_text("remotes.json").await {
            Ok(text) => match RemoteManifest::parse(&text) {
                Ok(parsed) => Manifest::Ready(parsed),
                Err(error) => Manifest::Failed(error.to_string()),
            },
            Err(error) => Manifest::Failed(error),
        };
        manifest.set(loaded);
    });

    let remote_links = move || {
        match manifest.get() {
        Manifest::Ready(manifest) => manifest
            .remotes
            .iter()
            .map(|entry| {
                let name = entry.name.clone();
                let href = format!("#/{name}");
                let selected = move || active.get().as_deref() == Some(name.as_str());
                view! {
                    <a href=href class=move || if selected() { "nav-link selected" } else { "nav-link" }>
                        {entry.title.clone()}
                    </a>
                }
            })
            .collect_view()
            .into_any(),
        Manifest::Loading => view! { <span class="nav-note">"loading remotes…"</span> }.into_any(),
        Manifest::Failed(reason) => view! { <span class="nav-note error">{reason}</span> }.into_any(),
    }
    };

    view! {
        <header class="bar">
            <a class="brand" href="#/">"qq"</a>
            <nav class="remotes">{remote_links}</nav>
            <span class="protocol" title="Protocol range this build of the UI supports">
                {format!("protocol {}", protocol_range_label())}
            </span>
        </header>
        <main class="frame">
            <ServersPane servers=servers />
            <Outlet manifest=manifest active=active servers=servers />
        </main>
    }
}

/// Shell-owned: add a server by address and credential. Both stay in this
/// component's memory; nothing is persisted or placed in the URL.
#[component]
fn ServersPane(servers: RwSignal<Vec<ServerConnection>, LocalStorage>) -> impl IntoView {
    let url = RwSignal::new(String::new());
    let credential = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error: RwSignal<Option<String>> = RwSignal::new(None);

    let connect = move || {
        if busy.get_untracked() {
            return;
        }
        if servers.with_untracked(Vec::len) >= MAX_SERVERS {
            error.set(Some(format!(
                "this tab already holds {MAX_SERVERS} servers"
            )));
            return;
        }
        let base_url = url.get_untracked();
        let secret = credential.get_untracked();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match probe(&base_url, &secret).await {
                Ok(connection) => {
                    let id = connection.server_info().server_id;
                    servers.update(|list| {
                        list.retain(|known| known.server_info().server_id != id);
                        list.push(connection);
                    });
                    url.set(String::new());
                    credential.set(String::new());
                }
                Err(failure) => error.set(Some(failure.to_string())),
            }
            busy.set(false);
        });
    };

    let remove = move |index: usize| {
        servers.update(|list| {
            if index < list.len() {
                list.remove(index);
            }
        });
    };

    view! {
        <aside class="servers">
            <h2>"Servers"</h2>
            <ul class="server-list">
                <For
                    each=move || servers.with(|list| (0..list.len()).collect::<Vec<_>>())
                    key=|index| *index
                    children=move |index| {
                        let info = move || servers.with(|list| list.get(index).map(|connection| (
                            connection.server_info().display_name.clone(),
                            connection.server_info().version.clone(),
                            connection.base_url().to_owned(),
                        )));
                        view! {
                            <li class="server">
                                <span class="dot live"></span>
                                <span class="server-name">{move || info().map(|(name, _, _)| name)}</span>
                                <button class="link" on:click=move |_| remove(index)>"remove"</button>
                                <span class="server-meta">{move || info().map(|(_, version, url)| format!("qq {version} · {url}"))}</span>
                            </li>
                        }
                    }
                />
            </ul>
            <form class="connect" on:submit=move |event| { event.prevent_default(); connect(); }>
                <label>
                    "Server URL"
                    <input
                        type="url"
                        placeholder="https://build-box.tail1234.ts.net"
                        autocomplete="off"
                        prop:value=move || url.get()
                        on:input=move |event| url.set(event_target_value(&event))
                    />
                </label>
                <label>
                    "Credential"
                    <input
                        type="password"
                        autocomplete="off"
                        prop:value=move || credential.get()
                        on:input=move |event| credential.set(event_target_value(&event))
                    />
                </label>
                <button type="submit" disabled=move || busy.get()>
                    {move || if busy.get() { "connecting…" } else { "connect" }}
                </button>
                <p class="error" role="alert">{move || error.get().unwrap_or_default()}</p>
                <p class="note">
                    "Credentials stay in this tab's memory. Pairing and saved server profiles arrive with the Servers slice."
                </p>
            </form>
        </aside>
    }
}

/// The one element a remote renders into. Any change to the active remote or
/// the server list unmounts the current remote and mounts the new pair.
#[component]
fn Outlet(
    manifest: RwSignal<Manifest, LocalStorage>,
    active: RwSignal<Option<String>>,
    servers: RwSignal<Vec<ServerConnection>, LocalStorage>,
) -> impl IntoView {
    let root = NodeRef::<html::Div>::new();
    let status: RwSignal<Option<String>> = RwSignal::new(None);
    let loaded: StoredValue<HashMap<String, LoadedRemote>, LocalStorage> =
        StoredValue::new_local(HashMap::new());
    let mounted: StoredValue<Option<LoadedRemote>, LocalStorage> = StoredValue::new_local(None);
    let generation = StoredValue::new(0_u64);

    Effect::new(move |_| {
        let entry = match (manifest.get(), active.get()) {
            (Manifest::Ready(manifest), Some(name)) => manifest
                .remotes
                .iter()
                .find(|entry| entry.name == name)
                .cloned(),
            _ => None,
        };
        let config =
            RemoteConfig::new(servers.with(|list| list.iter().map(remote_server).collect()));
        let Some(root_element) = root.get() else {
            return;
        };
        let this = generation.with_value(|generation| generation + 1);
        generation.set_value(this);

        if let Some(previous) = mounted.with_value(Option::clone) {
            previous.unmount();
            mounted.set_value(None);
        }
        root_element.set_inner_html("");

        let Some(entry) = entry else {
            status.set(match active.get_untracked() {
                Some(name) if matches!(manifest.get_untracked(), Manifest::Ready(_)) => {
                    Some(format!("no remote named {name:?} in remotes.json"))
                }
                _ => None,
            });
            return;
        };
        status.set(Some(format!("loading {}…", entry.title)));
        spawn_local(async move {
            let remote = match loaded.with_value(|cache| cache.get(&entry.name).cloned()) {
                Some(remote) => remote,
                None => match LoadedRemote::load(&entry).await {
                    Ok(remote) => {
                        loaded.update_value(|cache| {
                            cache.insert(entry.name.clone(), remote.clone());
                        });
                        remote
                    }
                    Err(error) => {
                        if generation.get_value() == this {
                            status.set(Some(error.to_string()));
                        }
                        return;
                    }
                },
            };
            if generation.get_value() != this {
                return;
            }
            match remote.mount(&root_element, &config) {
                Ok(()) => {
                    mounted.set_value(Some(remote));
                    status.set(None);
                }
                Err(error) => status.set(Some(error.to_string())),
            }
        });
    });

    let welcome = move || {
        (active.get().is_none() && status.get().is_none()).then(|| view! {
            <div class="welcome">
                <h1>"qq"</h1>
                <p>"Connect a server on the left, then open a remote from the top bar."</p>
                <p class="note">
                    "Each remote is an independently deployed wasm module; the shell only knows the contract "
                    <code>"mount(root, config) / unmount()"</code> " and " <code>"remotes.json"</code> "."
                </p>
            </div>
        })
    };

    view! {
        <section class="outlet-frame">
            <p class="outlet-status" class:hidden=move || status.get().is_none()>
                {move || status.get().unwrap_or_default()}
            </p>
            {welcome}
            <div class="outlet" node_ref=root></div>
        </section>
    }
}

fn remote_server(connection: &ServerConnection) -> RemoteServer {
    let info = connection.server_info();
    RemoteServer {
        server_id: info.server_id.to_string(),
        display_name: info.display_name.clone(),
        base_url: connection.base_url().to_owned(),
        credential: connection.expose_credential().to_owned(),
    }
}

fn active_remote_from_hash() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    let name = hash.trim_start_matches('#').trim_start_matches('/');
    (!name.is_empty()).then(|| name.to_owned())
}

fn protocol_range_label() -> String {
    let (start, end) = (SUPPORTED_PROTOCOL.start(), SUPPORTED_PROTOCOL.end());
    if start == end {
        start.to_string()
    } else {
        format!("{start}–{end}")
    }
}

/// `GET` of a same-origin, shell-relative asset such as `remotes.json`.
async fn fetch_text(relative: &str) -> Result<String, String> {
    let Some(window) = web_sys::window() else {
        return Err("no window".to_owned());
    };
    let base = window
        .document()
        .and_then(|document| document.base_uri().ok().flatten())
        .unwrap_or_default();
    let url = match web_sys::Url::new_with_base(relative, &base) {
        Ok(url) => url.href(),
        Err(_) => return Err(format!("cannot resolve {relative}")),
    };
    let response = match reqwest::get(&url).await {
        Ok(response) => response,
        Err(error) => return Err(format!("{relative}: {error}")),
    };
    if !response.status().is_success() {
        return Err(format!("{relative}: HTTP {}", response.status().as_u16()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("{relative}: {error}"))
}
