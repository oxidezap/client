use std::sync::Arc;

use oxidezap_ipc::{CallAction, ClientRequest, Request};

use super::Wire;
use crate::video::RecoverySink;

pub(super) fn sink(wire: Wire) -> std::io::Result<RecoverySink> {
    let send = imp::sender(wire)?;
    Ok(Arc::new(move |call_id, stream| {
        let request = Request::bare(ClientRequest::Call(CallAction::RequestVideoKeyframe {
            call_id: call_id.to_owned(),
            stream,
        }));
        serde_json::to_vec(&request).is_ok_and(&send)
    }))
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::Wire;

    pub(super) fn sender(wire: Wire) -> std::io::Result<impl Fn(Vec<u8>) -> bool + Send + Sync> {
        let (outgoing, incoming) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        std::thread::Builder::new()
            .name("oxidezap-video-recovery".into())
            .spawn(move || {
                while let Ok(frame) = incoming.recv() {
                    if wire.send_line(&frame).is_err() {
                        break;
                    }
                }
            })?;
        Ok(move |frame| outgoing.try_send(frame).is_ok())
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::Wire;

    pub(super) fn sender(wire: Wire) -> std::io::Result<impl Fn(Vec<u8>) -> bool + Send + Sync> {
        Ok(move |frame: Vec<u8>| {
            // Never wait behind teardown, and never retain a Link past it.
            let Ok(link) = wire.0.try_lock() else {
                return false;
            };
            link.as_ref()
                .is_some_and(|link| link.send_line(&frame).is_ok())
        })
    }
}
