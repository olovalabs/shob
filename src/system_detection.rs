#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub cpu: Option<u8>,
    pub memory: Option<u8>,
    pub gpu: Option<u8>,
    pub memory_used: u64,
    pub memory_total: u64,
}

pub struct Sampler {
    system: sysinfo::System,
}

impl Sampler {
    pub fn new() -> Self {
        let mut system = sysinfo::System::new();
        system.refresh_cpu_usage();
        Self { system }
    }

    pub fn sample(&mut self) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        let total = self.system.total_memory();
        let cpu = Some(self.system.global_cpu_usage().round().clamp(0.0, 100.0) as u8);
        Snapshot {
            cpu,
            memory: (total > 0).then(|| {
                ((self.system.used_memory() as f64 / total as f64 * 100.0)
                    .round()
                    .clamp(0.0, 100.0)) as u8
            }),
            gpu: gpu_usage(),
            memory_used: self.system.used_memory(),
            memory_total: total,
        }
    }
}

#[cfg(target_os = "linux")]
fn gpu_usage() -> Option<u8> {
    (0..16)
        .filter_map(|index| {
            let path = format!("/sys/class/drm/card{index}/device/gpu_busy_percent");
            std::fs::read_to_string(path)
                .ok()?
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|value| *value <= 100)
        })
        .max()
}

#[cfg(not(target_os = "linux"))]
fn gpu_usage() -> Option<u8> {
    None
}
