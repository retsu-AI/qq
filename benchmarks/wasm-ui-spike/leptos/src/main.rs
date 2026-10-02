//! ADR-0017 spike: the Leptos candidate. Same feed, same reducer, same view
//! as the Dioxus spike; only the rendering layer differs.

#![forbid(unsafe_code)]

use std::{any::Any, cell::RefCell};

use leptos::{mount::mount_to, prelude::*, task::spawn_local};
use qq_client::SessionClient;
use qq_protocol::SessionId;
use spike_common::{
    Feed, FeedUpdate, SpikeConfig, connect, connection_label, fetch_body, now_ms, run_feed,
    start_frame_monitor,
};
use wasm_bindgen::prelude::*;

thread_local! {
    static MOUNTED: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

/// Remote contract, host side: `mount(element, configJson)` renders the
/// remote into `element`; `unmount()` tears it down. Exported from the
/// wasm-bindgen module trunk emits, so a plain `import()` of the module is
/// all a host needs.
#[wasm_bindgen]
pub fn mount(root: web_sys::HtmlElement, config: &str) -> Result<(), JsValue> {
    let config = SpikeConfig::from_json(config)
        .ok_or_else(|| JsValue::from_str("mount: config needs server, credential, workspace"))?;
    unmount();
    let handle = mount_to(root, move || {
        view! { <App config=config.clone() embedded=true /> }
    });
    MOUNTED.with(|slot| *slot.borrow_mut() = Some(Box::new(handle)));
    Ok(())
}

#[wasm_bindgen]
pub fn unmount() {
    MOUNTED.with(|slot| slot.borrow_mut().take());
}

fn main() {
    let document = web_sys::window().and_then(|window| window.document());
    let root = document.and_then(|document| document.get_element_by_id("spike-standalone"));
    let (Some(root), Some(config)) = (root, SpikeConfig::from_location()) else {
        return;
    };
    let handle = mount_to(root.unchecked_into(), move || {
        view! { <App config=config.clone() embedded=false /> }
    });
    handle.forget();
}

#[component]
fn App(config: SpikeConfig, embedded: bool) -> impl IntoView {
    let feed = RwSignal::new_local(Feed::default());
    let client: StoredValue<Option<SessionClient>, LocalStorage> = StoredValue::new_local(None);

    start_frame_monitor(move |frames| feed.update(|feed| feed.frames = frames));
    spawn_local(async move {
        match connect(&config).await {
            Ok(connected) => {
                client.set_value(Some(connected.clone()));
                run_feed(connected, config, move |update| {
                    let started = now_ms();
                    let is_event = matches!(update, FeedUpdate::Event(_));
                    feed.update(|feed| {
                        feed.apply(update);
                        if is_event {
                            feed.apply.record(started, now_ms());
                        }
                    });
                })
                .await;
            }
            Err(error) => feed.update(|feed| feed.apply(FeedUpdate::Failed(error.to_string()))),
        }
    });

    let focus = move |id: SessionId| {
        let needs_body = feed.try_update(|feed| feed.focus(id)).unwrap_or(false);
        let Some(workspace_id) = feed.with_untracked(|feed| feed.workspace_id) else {
            return;
        };
        let Some(client) = client.get_value() else {
            return;
        };
        if needs_body {
            spawn_local(async move {
                match fetch_body(&client, workspace_id, id).await {
                    Ok(snapshot) => feed.update(|feed| feed.install(snapshot)),
                    Err(error) => feed.update(|feed| feed.error = Some(error.to_string())),
                }
            });
        }
    };

    view! {
        <div class=if embedded { "spike embedded" } else { "spike" }>
            <div class="status">
                <span class="fw">"Leptos 0.8 (CSR)"</span>
                <span>"conn " <b>{move || feed.with(|feed| connection_label(feed.connection))}</b></span>
                <span>"events " <b>{move || feed.with(|feed| feed.apply.events)}</b></span>
                <span>"ev/s " <b>{move || feed.with(|feed| format!("{:.0}", feed.apply.events_per_second()))}</b></span>
                <span>
                    "apply ms mean/max "
                    <b>{move || feed.with(|feed| format!("{:.3}/{:.1}", feed.apply.mean_ms(), feed.apply.max_ms))}</b>
                </span>
                <span>
                    "frames long/max "
                    <b>{move || feed.with(|feed| format!("{}/{:.0}ms", feed.frames.long_frames, feed.frames.max_frame_ms))}</b>
                </span>
                <span class="err">{move || feed.with(|feed| feed.error.clone().unwrap_or_default())}</span>
            </div>
            <div class="body">
                <div class="sessions">
                    <For
                        each=move || feed.with(|feed| feed.rows().into_iter().map(|row| row.id).collect::<Vec<_>>())
                        key=|id| *id
                        children=move |id| {
                            let row = move || {
                                feed.with(|feed| feed.rows().into_iter().find(|row| row.id == id))
                            };
                            view! {
                                <div
                                    class=move || {
                                        if feed.with(|feed| feed.focused == Some(id)) { "row focused" } else { "row" }
                                    }
                                    style=move || format!("padding-left: {}px", 12 + 16 * row().map_or(0, |row| row.depth))
                                    on:click=move |_| focus(id)
                                >
                                    <span class="group">{move || row().map(|row| row.group.label())}</span>
                                    " "
                                    {move || row().map(|row| row.title)}
                                    " "
                                    <span class="tail">{move || row().map(|row| row.tail)}</span>
                                </div>
                            }
                        }
                    />
                </div>
                <div class="transcript">
                    <For
                        each=move || feed.with(|feed| feed.lines().into_iter().map(|line| line.id).collect::<Vec<_>>())
                        key=|id| *id
                        children=move |id| {
                            let line = move || {
                                feed.with(|feed| feed.lines().into_iter().find(|line| line.id == id))
                            };
                            view! {
                                <p class=move || {
                                    let line = line();
                                    let user = line.as_ref().is_some_and(|line| line.user);
                                    let streaming = line.as_ref().is_some_and(|line| line.streaming);
                                    format!(
                                        "msg{}{}",
                                        if user { " user" } else { "" },
                                        if streaming { " streaming" } else { "" },
                                    )
                                }>
                                    <span class="role">{move || if line().is_some_and(|line| line.user) { "you" } else { "assistant" }}</span>
                                    "\n"
                                    {move || line().map(|line| line.text)}
                                </p>
                            }
                        }
                    />
                </div>
            </div>
        </div>
    }
}
