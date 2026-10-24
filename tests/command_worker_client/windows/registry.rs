//! Real host-registry fixture; absolute HKU paths avoid HKCU virtualization.
use super::*;
use winsafe::{HKEY, RegistryValue, co};

pub(super) struct HostSecret {
    subkey: String,
    absolute: String,
}
impl HostSecret {
    pub(super) fn create(workspace: &Path) -> Self {
        let subkey = format!(
            r"Software\qq-registry-test-{}",
            ToolCallId::generate().unwrap()
        );
        let (key, _) = HKEY::CURRENT_USER
            .RegCreateKeyEx(
                &subkey,
                None,
                co::REG_OPTION::VOLATILE,
                co::KEY::ALL_ACCESS,
                None,
            )
            .unwrap();
        // Preserve the ordinary host user's inherited ACL. No package or
        // capability ACE is added to make the denial test artificially private.
        key.RegSetValueEx(
            Some("secret"),
            RegistryValue::Sz("host registry secret".to_owned()),
        )
        .unwrap();
        let absolute = format!(
            r"{}\{}",
            win_custody::current_user_sid_string().unwrap(),
            subkey
        );
        let secret = Self { subkey, absolute };
        secret.assert_unchanged();
        std::fs::write(workspace.join("host-registry-path"), &secret.absolute).unwrap();
        secret
    }
    pub(super) fn assert_unchanged(&self) {
        assert!(matches!(
            HKEY::USERS.RegGetValue(Some(&self.absolute), Some("secret"), co::RRF::RT_ANY).unwrap(),
            RegistryValue::Sz(value) if value == "host registry secret"
        ));
    }
}
impl Drop for HostSecret {
    fn drop(&mut self) {
        let _ = HKEY::CURRENT_USER.RegDeleteTree(Some(&self.subkey));
    }
}

pub(super) fn assert_host_secret_denied() {
    let path = std::fs::read_to_string("host-registry-path").unwrap();
    assert!(
        HKEY::USERS
            .RegGetValue(Some(&path), Some("secret"), co::RRF::RT_ANY)
            .is_err(),
        "LPAC read a host registry secret"
    );
    assert!(
        HKEY::USERS
            .RegOpenKeyEx(Some(&path), co::REG_OPTION::default(), co::KEY::SET_VALUE)
            .is_err(),
        "LPAC obtained write access to a host registry secret"
    );
}
