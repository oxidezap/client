//! File drops from the window or browser document.

use std::path::PathBuf;

use crate::app::WhatsAppApp;
use crate::platform::picker::Chosen;
use gpui::WeakEntity;

pub struct Listener(imp::Listener);

pub fn install(entity: WeakEntity<WhatsAppApp>, cx: gpui::AsyncApp) -> Result<Listener, String> {
    imp::install(entity, cx).map(Listener)
}

pub fn read_paths(paths: Vec<PathBuf>) -> Result<Chosen, String> {
    imp::read_paths(paths)
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::*;

    pub struct Listener;

    pub fn install(
        _entity: WeakEntity<WhatsAppApp>,
        _cx: gpui::AsyncApp,
    ) -> Result<Listener, String> {
        Ok(Listener)
    }

    pub fn read_paths(paths: Vec<PathBuf>) -> Result<Chosen, String> {
        Ok(crate::platform::picker::read_paths(&paths))
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    use js_sys::Uint8Array;
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;
    use wasm_bindgen_futures::JsFuture;

    pub struct Listener {
        document: web_sys::Document,
        dragover: Closure<dyn FnMut(web_sys::DragEvent)>,
        drop: Closure<dyn FnMut(web_sys::DragEvent)>,
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let target: &web_sys::EventTarget = self.document.as_ref();
            let _ = target.remove_event_listener_with_callback(
                "dragover",
                self.dragover.as_ref().unchecked_ref(),
            );
            let _ = target
                .remove_event_listener_with_callback("drop", self.drop.as_ref().unchecked_ref());
        }
    }

    pub fn install(
        entity: WeakEntity<WhatsAppApp>,
        cx: gpui::AsyncApp,
    ) -> Result<Listener, String> {
        let document = web_sys::window()
            .and_then(|window| window.document())
            .ok_or_else(|| "the browser document is unavailable".to_string())?;
        let dragover = Closure::new(|event: web_sys::DragEvent| event.prevent_default());
        let drop_entity = entity;
        let mut app = cx;
        let drop = Closure::new(move |event: web_sys::DragEvent| {
            event.prevent_default();
            let Some(files) = event.data_transfer().and_then(|data| data.files()) else {
                return;
            };
            let files = (0..files.length())
                .filter_map(|index| files.get(index))
                .collect::<Vec<_>>();
            let entity = drop_entity.clone();
            let Some((jid, reply)) = entity
                .update(&mut app, |app, cx| app.prepare_file_drop(cx))
                .ok()
                .flatten()
            else {
                return;
            };
            let mut task_app = app.clone();
            app.foreground_executor()
                .spawn(async move {
                    let chosen = read_files(files).await;
                    let _ = entity.update(&mut task_app, |app, cx| {
                        app.finish_attaching(&jid, reply, Ok(chosen), cx)
                    });
                })
                .detach();
        });
        let target: &web_sys::EventTarget = document.as_ref();
        target
            .add_event_listener_with_callback("dragover", dragover.as_ref().unchecked_ref())
            .map_err(|e| format!("could not listen for file drags: {e:?}"))?;
        target
            .add_event_listener_with_callback("drop", drop.as_ref().unchecked_ref())
            .map_err(|e| format!("could not listen for file drops: {e:?}"))?;
        Ok(Listener {
            document,
            dragover,
            drop,
        })
    }

    pub fn read_paths(_paths: Vec<PathBuf>) -> Result<Chosen, String> {
        Err("native file paths are unavailable in the browser".to_string())
    }

    pub async fn read_files(files: Vec<web_sys::File>) -> Chosen {
        let mut chosen = Chosen::default();
        let mut budget = crate::platform::picker::new_budget();
        for file in files {
            let file_name = file.name();
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "File.size is a non-negative byte count"
            )]
            let size = file.size() as u64;
            if let Some(refusal) = budget.refuse(&file_name, size) {
                chosen.refused.push(refusal);
                continue;
            }
            match JsFuture::from(file.array_buffer()).await {
                Ok(buffer) => {
                    let bytes = Uint8Array::new(&buffer).to_vec();
                    budget.took(bytes.len() as u64);
                    chosen.files.push(crate::platform::picker::Picked {
                        mime_type: if file.type_().is_empty() {
                            crate::platform::picker::mime_for_name(&file_name).to_string()
                        } else {
                            file.type_()
                        },
                        file_name,
                        bytes,
                    });
                }
                Err(error) => chosen
                    .refused
                    .push(format!("{file_name} could not be read ({error:?}).")),
            }
        }
        chosen
    }
}

#[cfg(target_family = "wasm")]
pub use imp::read_files;

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn a_dropped_file_enters_the_shared_picker_result() {
        let path = std::env::temp_dir().join(format!(
            "oxidezap-drop-test-{}-upload.bin",
            std::process::id()
        ));
        let expected_name = path
            .file_name()
            .expect("test path has a file name")
            .to_string_lossy()
            .into_owned();
        std::fs::write(&path, b"test payload").expect("write test file");

        let chosen = super::read_paths(vec![PathBuf::from(&path)]).expect("read dropped file");
        std::fs::remove_file(path).expect("remove test file");

        assert_eq!(chosen.files.len(), 1);
        assert_eq!(chosen.files[0].file_name, expected_name);
        assert_eq!(chosen.files[0].bytes, b"test payload");
    }

    #[test]
    fn an_unreadable_dropped_path_surfaces_as_an_error() {
        let chosen = super::read_paths(vec![PathBuf::from("/path/that/does/not/exist")])
            .expect("picker returns refusals per file");

        assert!(chosen.files.is_empty());
        assert_eq!(chosen.refused.len(), 1);
    }
}
