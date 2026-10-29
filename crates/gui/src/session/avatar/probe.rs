//! Opt-in synthetic local-cache restore measurement. No account or network.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as StdOrdering};
use std::time::Duration;

fn cpu_ticks() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            let (_, fields) = stat.rsplit_once(") ")?;
            let values: Vec<_> = fields.split_whitespace().collect();
            Some(values.get(11)?.parse::<u64>().ok()? + values.get(12)?.parse::<u64>().ok()?)
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "synthetic restore measurement; run explicitly with --nocapture"]
fn native_restore_probe() {
    const ROOT: &str = "OXIDEZAP_AVATAR_PROBE_ROOT";
    let Some(root) = std::env::var_os(ROOT) else {
        let root = std::env::temp_dir().join(format!("avatar-probe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for _ in 0..3 {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::avatar::probe::native_restore_probe",
                    "--ignored",
                    "--nocapture",
                ])
                .env(ROOT, &root)
                .env("XDG_RUNTIME_DIR", &root)
                .env("LOCALAPPDATA", &root)
                .env("OXIDEZAP_ACCOUNT", "synthetic-avatar-probe")
                .env("SMOL_THREADS", "1")
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut bytes = std::io::Cursor::new(Vec::new());
    let jpeg = std::env::var("OXIDEZAP_AVATAR_PROBE_FORMAT").as_deref() == Ok("jpeg");
    let edge = if jpeg { 512 } else { 1024 };
    // A JPEG preview and a larger PNG stress the real decoder/thumbnail path.
    let image = image::RgbaImage::from_fn(edge, edge, |x, y| {
        image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 239) as u8, 255])
    });
    let image = if jpeg {
        image::DynamicImage::ImageRgb8(image::DynamicImage::ImageRgba8(image).to_rgb8())
    } else {
        image::DynamicImage::ImageRgba8(image)
    };
    image
        .write_to(
            &mut bytes,
            if jpeg {
                image::ImageFormat::Jpeg
            } else {
                image::ImageFormat::Png
            },
        )
        .unwrap();
    let bytes = bytes.into_inner();
    let chats: Vec<_> = (0..25)
        .map(|i| {
            let key = format!("synthetic-avatar-probe-{i}");
            let path = oxidezap_ipc::media_path(&key).unwrap();
            assert!(path.starts_with(&root));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &bytes).unwrap();
            ChatDemand {
                jid: format!("synthetic-{i}@example.invalid"),
                picture_id: Some("picture".into()),
                cache_key: Some(key),
            }
        })
        .collect();
    clear_image_sources();
    let finished = Arc::new(AtomicBool::new(false));
    let gap_us = Arc::new(AtomicU64::new(0));
    let heartbeat = {
        let finished = finished.clone();
        let gap_us = gap_us.clone();
        smol::spawn(async move {
            let mut previous = Instant::now();
            while !finished.load(StdOrdering::Relaxed) {
                smol::Timer::after(Duration::from_millis(1)).await;
                let now = Instant::now();
                gap_us.fetch_max(
                    now.saturating_duration_since(previous).as_micros() as u64,
                    StdOrdering::Relaxed,
                );
                previous = now;
            }
        })
    };
    // Warm the async executor before starting the measurement.
    smol::block_on(smol::Timer::after(Duration::from_millis(10)));
    let before_io = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    let before_cpu = cpu_ticks();
    let before_reads = native::PERSISTENT_READS.load(StdOrdering::Relaxed);
    let manager = AvatarManager::new();
    let start = Instant::now();
    let mut epoch = manager.demand_epoch();
    manager.demand_chats_windowed(&chats[..15], &chats[15..]);
    let mut first = None;
    let mut visible = None;
    loop {
        // Simulate viewport demand after a cache-completion notification.
        let next_epoch = manager.demand_epoch();
        if next_epoch != epoch {
            manager.demand_chats_windowed(&chats[..15], &chats[15..]);
            epoch = next_epoch;
        }
        let loaded: Vec<_> = chats
            .iter()
            .map(|c| get_avatar_image(c.cache_key.as_ref().unwrap()).is_some())
            .collect();
        let elapsed = start.elapsed();
        if first.is_none() && loaded[..15].iter().any(|x| *x) {
            first = Some(elapsed);
        }
        if visible.is_none() && loaded[..15].iter().all(|x| *x) {
            visible = Some(elapsed);
        }
        if loaded.iter().all(|x| *x) {
            break;
        }
        assert!(elapsed < Duration::from_secs(60), "restore did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
    let complete = start.elapsed();
    finished.store(true, StdOrdering::Relaxed);
    smol::block_on(heartbeat);
    // Publication follows insertion into RAM; allow that final task to drain.
    smol::block_on(smol::Timer::after(Duration::from_millis(10)));
    let demands = manager.take_demands();
    assert_eq!(demands.len(), 25);
    assert!(demands.iter().all(|d| !d.need_bytes && !d.cache_miss));
    assert_eq!(
        native::PERSISTENT_READS.load(StdOrdering::Relaxed) - before_reads,
        25
    );
    let warm = Instant::now();
    for _ in 0..10 {
        manager.demand_chats_windowed(&chats[..15], &chats[15..]);
    }
    assert!(manager.take_demands().is_empty());
    assert_eq!(
        native::PERSISTENT_READS.load(StdOrdering::Relaxed) - before_reads,
        25,
        "warm viewport must not reread disk"
    );
    println!(
        "restore first_us={} visible_us={} all_us={} executor_gap_us={} warm_10_us={} freshness={} byte_demands=0",
        first.unwrap().as_micros(),
        visible.unwrap().as_micros(),
        complete.as_micros(),
        gap_us.load(StdOrdering::Relaxed),
        warm.elapsed().as_micros(),
        demands.len()
    );
    println!("fixture={} edge={edge}", if jpeg { "jpeg" } else { "png" });
    println!(
        "io_before={before_io:?} io_after={:?}",
        std::fs::read_to_string("/proc/self/io").unwrap_or_default()
    );
    println!(
        "cpu_ticks={} persistent_reads=25",
        cpu_ticks().saturating_sub(before_cpu)
    );
    println!(
        "process={:?}",
        std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:"))
            .collect::<Vec<_>>()
    );
}
