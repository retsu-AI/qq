//! Host side of the remote contract: dynamic `import()` of a remote's ES
//! module, its wasm-bindgen initializer, then `mount`/`unmount` through
//! `Reflect`. The `import()` itself lives in a two-line script in
//! `index.html` because wasm-bindgen cannot express a dynamic import.

use js_sys::{Function, Promise, Reflect};
use qq_ui_common::remote::{RemoteConfig, RemoteEntry};
use thiserror::Error;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    /// `window.qqShell.importRemote(moduleUrl, wasmUrl)`: imports the module,
    /// awaits its default initializer with `{ module_or_path: wasmUrl }`, and
    /// resolves to the module namespace object.
    #[wasm_bindgen(js_namespace = qqShell, js_name = importRemote)]
    fn import_remote(module: &str, wasm: &str) -> Promise;
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum LoadError {
    #[error("could not load remote {name:?} from {module}: {reason}")]
    Import {
        name: String,
        module: String,
        reason: String,
    },
    #[error(
        "remote {name:?} does not export `{export}`; it does not implement the remote contract"
    )]
    MissingExport { name: String, export: &'static str },
    #[error("remote {name:?} refused to mount: {reason}")]
    Mount { name: String, reason: String },
}

/// A remote whose module has been imported and initialized once; `mount` and
/// `unmount` may be called any number of times.
#[derive(Clone)]
pub struct LoadedRemote {
    name: String,
    mount: Function,
    unmount: Function,
}

impl LoadedRemote {
    pub async fn load(entry: &RemoteEntry) -> Result<Self, LoadError> {
        let module = match JsFuture::from(import_remote(&entry.module, &entry.wasm)).await {
            Ok(module) => module,
            Err(error) => {
                return Err(LoadError::Import {
                    name: entry.name.clone(),
                    module: entry.module.clone(),
                    reason: describe(&error),
                });
            }
        };
        let export = |export: &'static str| -> Result<Function, LoadError> {
            Reflect::get(&module, &JsValue::from_str(export))
                .ok()
                .and_then(|value| value.dyn_into::<Function>().ok())
                .ok_or_else(|| LoadError::MissingExport {
                    name: entry.name.clone(),
                    export,
                })
        };
        Ok(Self {
            name: entry.name.clone(),
            mount: export("mount")?,
            unmount: export("unmount")?,
        })
    }

    pub fn mount(
        &self,
        root: &web_sys::HtmlElement,
        config: &RemoteConfig,
    ) -> Result<(), LoadError> {
        match self.mount.call2(
            &JsValue::UNDEFINED,
            root,
            &JsValue::from_str(&config.to_json()),
        ) {
            Ok(_) => Ok(()),
            Err(error) => Err(LoadError::Mount {
                name: self.name.clone(),
                reason: describe(&error),
            }),
        }
    }

    pub fn unmount(&self) {
        // A remote that throws on unmount has nothing further we can do for
        // it; the shell clears the outlet regardless.
        let _ = self.unmount.call0(&JsValue::UNDEFINED);
    }
}

/// A JS error's `message`, else its string form. Never includes a stack,
/// which could carry query strings.
fn describe(error: &JsValue) -> String {
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.message()),
        None => error
            .as_string()
            .unwrap_or_else(|| String::from("unknown error")),
    }
}
