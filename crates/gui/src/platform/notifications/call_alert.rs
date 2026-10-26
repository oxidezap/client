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
    // Audio startup errors cancel this token, not notification ownership.
    ringtone_cancelled: Arc<AtomicBool>,
    retired: Arc<AtomicBool>,
    ringtone: Option<oxidezap_audio::RingtonePlayback>,
}

impl IncomingCallAlert {
    pub fn claim(
        account: AccountId,
        tag: String,
        cancelled: Arc<AtomicBool>,
        ringtone_cancelled: Arc<AtomicBool>,
        retired: Arc<AtomicBool>,
    ) -> std::io::Result<Option<Self>> {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        Ok(imp::claim(account, tag)?.map(|owner| Self {
            ownership: owner,
            cancelled,
            ringtone_cancelled,
            retired,
            ringtone: None,
        }))
    }

    pub fn play(&mut self, preferences: CallDevicePreferences) -> anyhow::Result<()> {
        self.play_with(preferences, oxidezap_audio::start_ringtone_cancellable)
    }

    fn play_with(
        &mut self,
        preferences: CallDevicePreferences,
        start: impl FnOnce(
            Option<String>,
            oxidezap_core::Ringtone,
            Arc<AtomicBool>,
        ) -> anyhow::Result<oxidezap_audio::RingtonePlayback>,
    ) -> anyhow::Result<()> {
        if !self.cancelled.load(Ordering::Acquire) {
            self.ringtone = Some(start(
                preferences.ringtone_output,
                preferences.ringtone,
                self.ringtone_cancelled.clone(),
            )?);
        }
        Ok(())
    }
}

impl Drop for IncomingCallAlert {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.ringtone_cancelled.store(true, Ordering::Release);
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
        fn call_alert_ringtone_failure_keeps_the_notification_owner() {
            use std::sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            };
            let path = std::env::temp_dir().join(format!(
                "oxidezap-call-alert-failure-{}-{}",
                std::process::id(),
                wacore::time::now_utc().timestamp_nanos_opt().unwrap()
            ));
            let cancelled = Arc::new(AtomicBool::new(false));
            let ringtone_cancelled = Arc::new(AtomicBool::new(false));
            let mut alert = super::super::IncomingCallAlert {
                ownership: claim_at(&path, "ring".into()).unwrap().unwrap(),
                cancelled: cancelled.clone(),
                ringtone_cancelled: ringtone_cancelled.clone(),
                retired: Arc::new(AtomicBool::new(false)),
                ringtone: None,
            };
            let error = alert
                .play_with(Default::default(), |_, _, stop| {
                    // A failed audio opener drops its playback guard, cancelling
                    // startup. This must not cancel the surrounding notification.
                    stop.store(true, Ordering::Release);
                    anyhow::bail!("fixture output unavailable")
                })
                .unwrap_err();
            assert_eq!(error.to_string(), "fixture output unavailable");
            assert!(ringtone_cancelled.load(Ordering::Acquire));
            assert!(!cancelled.load(Ordering::Acquire));
            assert!(claim_at(&path, "ring".into()).unwrap().is_none());
            drop(alert);
            assert!(cancelled.load(Ordering::Acquire));
            assert!(claim_at(&path, "ring".into()).unwrap().is_some());
            std::fs::remove_file(path).unwrap();
        }

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
                ringtone_cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
