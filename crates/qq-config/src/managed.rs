use super::ConfigError;

pub(super) struct MdmConfiguration {
    pub(super) origin: String,
    pub(super) content: String,
}

#[cfg(any(
    test,
    feature = "test-support",
    target_os = "macos",
    target_os = "windows"
))]
impl MdmConfiguration {
    pub(super) fn new(origin: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            content: content.into(),
        }
    }
}

pub(super) trait MdmReader: Send + Sync {
    fn read(&self) -> Result<Option<MdmConfiguration>, ConfigError>;
}

#[cfg(feature = "test-support")]
pub(super) struct SequenceMdmReader {
    pub(super) origin: String,
    pub(super) contents: std::sync::Mutex<std::collections::VecDeque<String>>,
}

#[cfg(feature = "test-support")]
impl MdmReader for SequenceMdmReader {
    fn read(&self) -> Result<Option<MdmConfiguration>, ConfigError> {
        let mut contents = self.contents.lock().expect("test MDM sequence lock");
        let content = if contents.len() > 1 {
            contents.pop_front().expect("nonempty test MDM sequence")
        } else {
            contents
                .front()
                .expect("nonempty test MDM sequence")
                .clone()
        };
        Ok(Some(MdmConfiguration::new(self.origin.clone(), content)))
    }
}

pub(super) struct SystemMdmReader;

impl MdmReader for SystemMdmReader {
    fn read(&self) -> Result<Option<MdmConfiguration>, ConfigError> {
        read_system_mdm()
    }
}

#[cfg(target_os = "macos")]
fn read_system_mdm() -> Result<Option<MdmConfiguration>, ConfigError> {
    use objc2_core_foundation::{
        CFPreferencesAppValueIsForced, CFPreferencesCopyAppValue, CFString,
    };

    const APPLICATION: &str = "dev.qq";
    const KEY: &str = "ManagedConfig";
    const ORIGIN: &str = "macOS forced preference dev.qq/ManagedConfig";

    let application = CFString::from_static_str(APPLICATION);
    let key = CFString::from_static_str(KEY);
    if !CFPreferencesAppValueIsForced(&key, &application) {
        return Ok(None);
    }
    let value =
        CFPreferencesCopyAppValue(&key, &application).ok_or_else(|| ConfigError::MdmRead {
            origin: ORIGIN.to_owned(),
            message: "the forced preference disappeared while it was being read".to_owned(),
        })?;
    let content = value
        .downcast_ref::<CFString>()
        .ok_or_else(|| ConfigError::InvalidMdmValue {
            origin: ORIGIN.to_owned(),
        })?
        .to_string();
    Ok(Some(MdmConfiguration::new(ORIGIN, content)))
}

#[cfg(target_os = "windows")]
fn read_system_mdm() -> Result<Option<MdmConfiguration>, ConfigError> {
    use winreg::{HKLM, enums::RegType};

    const KEY: &str = r"Software\Policies\dev.qq";
    const VALUE: &str = "ManagedConfig";
    const ORIGIN: &str = r"Windows policy HKLM\Software\Policies\dev.qq\ManagedConfig";

    let policy = match HKLM.open_subkey(KEY) {
        Ok(policy) => policy,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(mdm_read_error(ORIGIN, error)),
    };
    let value = match policy.get_raw_value(VALUE) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(mdm_read_error(ORIGIN, error)),
    };
    if value.vtype != RegType::REG_SZ || !value.bytes.len().is_multiple_of(2) {
        return Err(ConfigError::InvalidMdmValue {
            origin: ORIGIN.to_owned(),
        });
    }

    let mut units: Vec<u16> = value
        .bytes
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    let content = String::from_utf16(&units).map_err(|_| ConfigError::InvalidMdmValue {
        origin: ORIGIN.to_owned(),
    })?;
    Ok(Some(MdmConfiguration::new(ORIGIN, content)))
}

#[cfg(target_os = "windows")]
fn mdm_read_error(origin: &str, error: std::io::Error) -> ConfigError {
    ConfigError::MdmRead {
        origin: origin.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_system_mdm() -> Result<Option<MdmConfiguration>, ConfigError> {
    Ok(None)
}
