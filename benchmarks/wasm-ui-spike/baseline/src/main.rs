//! ADR-0017 spike: the no-framework baseline. Same transport, reducer and
//! view model as the two candidates, rendered by rewriting `innerHTML`. Its
//! bundle size is the cost of qq-client + reqwest + the reducer alone, so the
//! framework overhead is `candidate - baseline`.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use spike_common::{
    Feed, FeedUpdate, SpikeConfig, connect, connection_label, now_ms, run_feed, start_frame_monitor,
};

fn main() {
    let document = web_sys::window().and_then(|window| window.document());
    let root = document.and_then(|document| document.get_element_by_id("spike-standalone"));
    let (Some(root), Some(config)) = (root, SpikeConfig::from_location()) else {
        return;
    };
    let feed = Rc::new(RefCell::new(Feed::default()));
    let render = {
        let feed = Rc::clone(&feed);
        move || root.set_inner_html(&html(&feed.borrow()))
    };
    let frames = {
        let feed = Rc::clone(&feed);
        let render = render.clone();
        move |stats| {
            feed.borrow_mut().frames = stats;
            render();
        }
    };
    start_frame_monitor(frames);
    wasm_bindgen_futures::spawn_local(async move {
        match connect(&config).await {
            Ok(client) => {
                run_feed(client, config, move |update| {
                    let started = now_ms();
                    let is_event = matches!(update, FeedUpdate::Event(_));
                    {
                        let mut feed = feed.borrow_mut();
                        feed.apply(update);
                        if is_event {
                            feed.apply.record(started, now_ms());
                        }
                    }
                    render();
                })
                .await;
            }
            Err(error) => {
                feed.borrow_mut()
                    .apply(FeedUpdate::Failed(error.to_string()));
                render();
            }
        }
    });
}

fn html(feed: &Feed) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(&format!(
        "<div class=\"spike\"><div class=\"status\"><span class=\"fw\">baseline (innerHTML)</span>\
         <span>conn <b>{}</b></span><span>events <b>{}</b></span><span>ev/s <b>{:.0}</b></span>\
         <span>apply ms mean/max <b>{:.3}/{:.1}</b></span><span>frames long/max <b>{}/{:.0}ms</b></span>\
         <span class=\"err\">{}</span></div><div class=\"body\"><div class=\"sessions\">",
        connection_label(feed.connection),
        feed.apply.events,
        feed.apply.events_per_second(),
        feed.apply.mean_ms(),
        feed.apply.max_ms,
        feed.frames.long_frames,
        feed.frames.max_frame_ms,
        escape(feed.error.as_deref().unwrap_or_default()),
    ));
    for row in feed.rows() {
        out.push_str(&format!(
            "<div class=\"row{}\" style=\"padding-left:{}px\"><span class=\"group\">{}</span> {} <span class=\"tail\">{}</span></div>",
            if feed.focused == Some(row.id) { " focused" } else { "" },
            12 + 16 * row.depth,
            row.group.label(),
            escape(&row.title),
            escape(&row.tail),
        ));
    }
    out.push_str("</div><div class=\"transcript\">");
    for line in feed.lines() {
        out.push_str(&format!(
            "<p class=\"msg{}{}\"><span class=\"role\">{}</span>\n{}</p>",
            if line.user { " user" } else { "" },
            if line.streaming { " streaming" } else { "" },
            if line.user { "you" } else { "assistant" },
            escape(&line.text),
        ));
    }
    out.push_str("</div></div></div>");
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
