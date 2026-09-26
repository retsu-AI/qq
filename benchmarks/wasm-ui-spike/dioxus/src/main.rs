//! ADR-0017 spike: the Dioxus candidate. Same feed, same reducer, same view
//! as the Leptos spike; only the rendering layer differs.

#![forbid(unsafe_code)]

use std::cell::RefCell;

use dioxus::prelude::*;
use futures_util::future::{AbortHandle, Abortable};
use qq_client::SessionClient;
use qq_protocol::SessionId;
use spike_common::{
    Feed, FeedUpdate, SpikeConfig, connect, connection_label, fetch_body, now_ms, run_feed,
    start_frame_monitor,
};
use wasm_bindgen::prelude::*;

thread_local! {
    static MOUNTED: RefCell<Option<(AbortHandle, web_sys::Element)>> = const { RefCell::new(None) };
}

/// Remote contract, host side: `mount(element, configJson)` renders the
/// remote into `element`; `unmount()` tears it down. `dioxus-web` exposes no
/// unmount for a launched root, so the spike drives `dioxus::web::run` itself
/// under an abort handle and clears the root's children on unmount.
#[wasm_bindgen]
pub fn mount(root: web_sys::HtmlElement, config: &str) -> Result<(), JsValue> {
    let config = SpikeConfig::from_json(config)
        .ok_or_else(|| JsValue::from_str("mount: config needs server, credential, workspace"))?;
    unmount();
    mount_into(root.into(), config, true);
    Ok(())
}

#[wasm_bindgen]
pub fn unmount() {
    if let Some((handle, root)) = MOUNTED.with(|slot| slot.borrow_mut().take()) {
        handle.abort();
        root.set_inner_html("");
    }
}

fn mount_into(root: web_sys::Element, config: SpikeConfig, embedded: bool) {
    let vdom = VirtualDom::new_with_props(App, AppProps { config, embedded });
    let (handle, registration) = AbortHandle::new_pair();
    let run = Abortable::new(
        dioxus::web::run(vdom, dioxus::web::Config::new().rootelement(root.clone())),
        registration,
    );
    wasm_bindgen_futures::spawn_local(async move {
        let _ = run.await;
    });
    MOUNTED.with(|slot| *slot.borrow_mut() = Some((handle, root)));
}

fn main() {
    let document = web_sys::window().and_then(|window| window.document());
    let root = document.and_then(|document| document.get_element_by_id("spike-standalone"));
    let (Some(root), Some(config)) = (root, SpikeConfig::from_location()) else {
        return;
    };
    mount_into(root, config, false);
}

#[component]
fn App(config: SpikeConfig, embedded: bool) -> Element {
    let mut feed = use_signal(Feed::default);
    let mut client: Signal<Option<SessionClient>> = use_signal(|| None);

    use_hook(move || {
        start_frame_monitor(move |frames| feed.write().frames = frames);
        spawn(async move {
            match connect(&config).await {
                Ok(connected) => {
                    client.set(Some(connected.clone()));
                    run_feed(connected, config, move |update| {
                        let started = now_ms();
                        let is_event = matches!(update, FeedUpdate::Event(_));
                        let mut feed = feed.write();
                        feed.apply(update);
                        if is_event {
                            feed.apply.record(started, now_ms());
                        }
                    })
                    .await;
                }
                Err(error) => feed.write().apply(FeedUpdate::Failed(error.to_string())),
            }
        });
    });

    let mut focus = move |id: SessionId| {
        let needs_body = feed.write().focus(id);
        let Some(workspace_id) = feed.peek().workspace_id else {
            return;
        };
        let Some(client) = client.peek().clone() else {
            return;
        };
        if needs_body {
            spawn(async move {
                match fetch_body(&client, workspace_id, id).await {
                    Ok(snapshot) => feed.write().install(snapshot),
                    Err(error) => feed.write().error = Some(error.to_string()),
                }
            });
        }
    };

    let feed = feed.read();
    let rows = feed.rows();
    let lines = feed.lines();
    rsx! {
        div { class: if embedded { "spike embedded" } else { "spike" },
            div { class: "status",
                span { class: "fw", "Dioxus 0.7 (web)" }
                span { "conn " b { "{connection_label(feed.connection)}" } }
                span { "events " b { "{feed.apply.events}" } }
                span { "ev/s " b { "{feed.apply.events_per_second():.0}" } }
                span { "apply ms mean/max " b { "{feed.apply.mean_ms():.3}/{feed.apply.max_ms:.1}" } }
                span { "frames long/max " b { "{feed.frames.long_frames}/{feed.frames.max_frame_ms:.0}ms" } }
                span { class: "err", "{feed.error.clone().unwrap_or_default()}" }
            }
            div { class: "body",
                div { class: "sessions",
                    for row in rows {
                        div {
                            key: "{row.id}",
                            class: if feed.focused == Some(row.id) { "row focused" } else { "row" },
                            style: "padding-left: {12 + 16 * row.depth}px",
                            onclick: move |_| focus(row.id),
                            span { class: "group", "{row.group.label()}" }
                            " {row.title} "
                            span { class: "tail", "{row.tail}" }
                        }
                    }
                }
                div { class: "transcript",
                    for line in lines {
                        p {
                            key: "{line.id}",
                            class: format!(
                                "msg{}{}",
                                if line.user { " user" } else { "" },
                                if line.streaming { " streaming" } else { "" },
                            ),
                            span { class: "role", if line.user { "you" } else { "assistant" } }
                            "\n{line.text}"
                        }
                    }
                }
            }
        }
    }
}
