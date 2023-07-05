use std::{borrow::Cow, path::PathBuf, sync::Arc, time::Duration};

use metric::{Attributes, U64Gauge};
use parking_lot::Mutex;
use sysinfo::{DiskExt, System, SystemExt};
use tokio::{self, task::JoinHandle};

/// Metrics that can be used to create a [`InstrumentedDiskProtection`].
#[derive(Debug)]
struct DiskProtectionMetrics {
    available_disk_space_percent: U64Gauge,
    directory: PathBuf,
}

impl DiskProtectionMetrics {
    /// Create a new [`DiskProtectionMetrics`].
    pub(crate) fn new(directory: PathBuf, registry: &metric::Registry) -> Self {
        let path: Cow<'static, str> = Cow::from(directory.display().to_string());
        let attributes = Attributes::from([("path", path)]);

        let available_disk_space_percent = registry
            .register_metric::<U64Gauge>(
                "disk_free_disk_space",
                "The percentage amount of disk available.",
            )
            .recorder(attributes);

        Self {
            available_disk_space_percent,
            directory,
        }
    }

    /// Measure the available disk space percentage.
    pub(crate) fn measure_available_disk_space_percent(&self, system: &mut System) {
        system.refresh_disks_list();

        let mut path = self.directory.clone();
        let fnd_disk = loop {
            if let Some(disk) = system
                .disks_mut()
                .iter_mut()
                .find(|disk| disk.mount_point() == path)
            {
                break Some(disk);
            }
            if !path.pop() {
                break None;
            }
        };

        if let Some(disk) = fnd_disk {
            disk.refresh();

            let available_disk: u64 = disk.available_space();
            let total_disk: u64 = disk.total_space();
            let available_disk_percentage =
                ((available_disk as f64) / (total_disk as f64) * 100.0).round() as u64;
            self.available_disk_space_percent
                .set(available_disk_percentage);
        }
    }
}

/// Disk Protection instrument.
pub struct InstrumentedDiskProtection {
    /// The metrics that are reported to the registry.
    metrics: DiskProtectionMetrics,
    /// The handle to terminate the background task.
    background_task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for InstrumentedDiskProtection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InstrumentedDiskProtection")
    }
}

impl InstrumentedDiskProtection {
    /// Create a new [`InstrumentedDiskProtection`].
    pub fn new(directory_to_track: PathBuf, registry: &metric::Registry) -> Self {
        let metrics = DiskProtectionMetrics::new(directory_to_track, registry);

        Self {
            metrics,
            background_task: Default::default(),
        }
    }

    /// Start the [`InstrumentedDiskProtection`] background task.
    pub async fn start(self) {
        let rc_self = Arc::new(self);
        let rc_self_clone = Arc::clone(&rc_self);

        *rc_self.background_task.lock() = Some(tokio::task::spawn(async move {
            rc_self_clone.background_task().await
        }));
    }

    /// Stop the [`InstrumentedDiskProtection`] background task.
    pub fn stop(&mut self) {
        if let Some(t) = self.background_task.lock().take() {
            t.abort()
        }
    }

    /// The background task that periodically performs the disk protection check.
    async fn background_task(&self) {
        let mut system = System::new_all();
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            self.metrics
                .measure_available_disk_space_percent(&mut system);

            interval.tick().await;
        }
    }
}

impl Drop for InstrumentedDiskProtection {
    fn drop(&mut self) {
        // future-proof, such that stop does not need to be explicitly called.
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use metric::Metric;

    use super::*;

    #[tokio::test]
    async fn test_metrics() {
        let registry = Arc::new(metric::Registry::new());

        struct MockAnyStruct;

        impl MockAnyStruct {
            pub(crate) async fn new(registry: &metric::Registry) -> Self {
                let disk_protection = InstrumentedDiskProtection::new(PathBuf::from("/"), registry);
                disk_protection.start().await;

                Self
            }
        }

        let _mock = MockAnyStruct::new(&registry).await;

        tokio::time::sleep(2 * Duration::from_secs(2)).await;

        let recorded_metric = registry
            .get_instrument::<Metric<U64Gauge>>("disk_free_disk_space")
            .expect("metric should exist")
            .get_observer(&Attributes::from(&[("path", "/")]))
            .expect("metric should have labels")
            .fetch();

        assert!(recorded_metric > 0_u64);
    }
}
