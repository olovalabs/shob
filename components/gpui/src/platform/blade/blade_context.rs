use anyhow::Context as _;
use blade_graphics as gpu;
use std::sync::Arc;
use util::ResultExt;

#[cfg_attr(target_os = "macos", derive(Clone))]
pub struct BladeContext {
    pub(super) gpu: Arc<gpu::Context>,
}

impl BladeContext {
    pub fn new() -> anyhow::Result<Self> {
        Self::init(device_id_filter(), true)
    }

    /// A context with no presentation ability, so blade never touches a surface,
    /// a window, or the display server. Everything still needs a working graphics
    /// driver.
    ///
    /// Each headless renderer gets its own device rather than sharing one. That
    /// costs a device creation per test, but a device that gets lost then only
    /// takes down the test that was using it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new_headless() -> anyhow::Result<Self> {
        Self::init(device_id_filter(), false)
    }

    fn init(device_id: Option<u32>, presentation: bool) -> anyhow::Result<Self> {
        let gpu = Arc::new(
            unsafe {
                gpu::Context::init(gpu::ContextDesc {
                    presentation,
                    validation: false,
                    device_id: device_id.unwrap_or(0),
                    ..Default::default()
                })
            }
            .map_err(|e| anyhow::anyhow!("{e:?}"))?,
        );
        Ok(Self { gpu })
    }
}

/// The PCI device id from `ZED_DEVICE_ID`, if it is set and valid.
fn device_id_filter() -> Option<u32> {
    match std::env::var("ZED_DEVICE_ID") {
        Ok(val) => parse_pci_id(&val)
            .context("Failed to parse device ID from `ZED_DEVICE_ID` environment variable")
            .log_err(),
        Err(std::env::VarError::NotPresent) => None,
        err => {
            err.context("Failed to read value of `ZED_DEVICE_ID` environment variable")
                .log_err();
            None
        }
    }
}

fn parse_pci_id(id: &str) -> anyhow::Result<u32> {
    let mut id = id.trim();

    if id.starts_with("0x") || id.starts_with("0X") {
        id = &id[2..];
    }
    let is_hex_string = id.chars().all(|c| c.is_ascii_hexdigit());
    let is_4_chars = id.len() == 4;
    anyhow::ensure!(
        is_4_chars && is_hex_string,
        "Expected a 4 digit PCI ID in hexadecimal format"
    );

    u32::from_str_radix(id, 16).context("parsing PCI ID as hex")
}

#[cfg(test)]
mod tests {
    use super::parse_pci_id;

    #[test]
    fn test_parse_device_id() {
        assert!(parse_pci_id("0xABCD").is_ok());
        assert!(parse_pci_id("ABCD").is_ok());
        assert!(parse_pci_id("abcd").is_ok());
        assert!(parse_pci_id("1234").is_ok());
        assert!(parse_pci_id("123").is_err());
        assert_eq!(
            parse_pci_id(&format!("{:x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:X}", 0x1234)).unwrap(),
        );

        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
    }
}
