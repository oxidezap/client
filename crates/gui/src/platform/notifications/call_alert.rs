use oxidezap_core::{AccountId, CallDevicePreferences};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub fn supported() -> bool {
    cfg!(all(windows, not(test)))
}

pub struct IncomingCallAlert {
    ownership: imp::Owner,
    cancelled: Arc<AtomicBool>,
    retired: Arc<AtomicBool>,
    ringtone: Option<oxidezap_audio::RingtonePlayback>,
}

impl IncomingCallAlert {
    pub fn claim(
        account: AccountId,
        tag: String,
        cancelled: Arc<AtomicBool>,
        retired: Arc<AtomicBool>,
    ) -> std::io::Result<Option<Self>> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        Ok(imp::claim(account, tag)?.map(|owner| Self {
            ownership: owner,
            cancelled,
            retired,
            ringtone: None,
        }))
    }

    pub fn play(&mut self, preferences: CallDevicePreferences) -> anyhow::Result<()> {
        if !self.cancelled.load(Ordering::Acquire) {
            self.ringtone = Some(oxidezap_audio::start_ringtone_cancellable(
                preferences.ringtone_output,
                preferences.ringtone,
                self.cancelled.clone(),
            )?);
        }
        Ok(())
    }
}

impl Drop for IncomingCallAlert {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.ringtone = None;
        if self.retired.load(Ordering::Acquire) {
            self.ownership.retire();
        }
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use oxidezap_core::AccountId;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read as _, Seek as _, Write as _};
    use std::path::Path;

    pub struct Owner {
        file: File,
        tag: String,
    }

    pub fn claim(account: AccountId, tag: String) -> io::Result<Option<Owner>> {
        let directory = oxidezap_ipc::state_dir()
            .ok_or_else(|| io::Error::other("call alert directory is unavailable"))?;
        claim_at(&directory.join(format!("call-alert-{account}.lock")), tag)
    }

    fn claim_at(path: &Path, tag: String) -> io::Result<Option<Owner>> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => {
                // A local answer can precede the other frontend's state frame.
                // Retain that decision on the locked inode to suppress a late ring.
                let mut retired = String::new();
                std::io::Read::by_ref(&mut file)
                    .take(128)
                    .read_to_string(&mut retired)?;
                Ok((retired != tag).then_some(Owner { file, tag }))
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    impl Owner {
        pub fn retire(&mut self) {
            let written = (|| -> io::Result<()> {
                self.file.rewind()?;
                self.file.set_len(0)?;
                self.file.write_all(self.tag.as_bytes())
            })();
            if let Err(error) = written {
                log::warn!("cannot retire incoming call alert: {error}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn call_alert_has_one_owner_and_releases_on_close() {
            let path = std::env::temp_dir().join(format!(
                "oxidezap-call-alert-test-{}-{}",
                std::process::id(),
                wacore::time::now_utc().timestamp_nanos_opt().unwrap()
            ));
            let owner = claim_at(&path, "call-one".into()).unwrap().unwrap();
            assert!(claim_at(&path, "call-one".into()).unwrap().is_none());
            drop(owner);
            assert!(claim_at(&path, "call-one".into()).unwrap().is_some());
            let owner = claim_at(&path, "call-one".into()).unwrap().unwrap();
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let guard = super::super::IncomingCallAlert {
                ownership: owner,
                cancelled: cancelled.clone(),
                retired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                ringtone: None,
            };
            drop(guard);
            assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
            assert!(claim_at(&path, "call-one".into()).unwrap().is_none());
            assert!(claim_at(&path, "call-two".into()).unwrap().is_some());
            std::fs::remove_file(path).unwrap();
        }
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    pub struct Owner;
    pub fn claim(_: oxidezap_core::AccountId, _: String) -> std::io::Result<Option<Owner>> {
        Ok(None)
    }
    impl Owner {
        pub fn retire(&mut self) {}
    }
}
