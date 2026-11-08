//! Which device a name, short id or fingerprint on the command line means. The service and
//! the command line both use this, so they never disagree about who is meant.

use linksy_core::{DeviceView, Fingerprint};

/// The device `target` names: a full fingerprint (taken as is), else a short id or a name,
/// ignoring case and the spaces around it. Exactly one must match.
pub fn resolve_device(devices: &[DeviceView], target: &str) -> Result<Fingerprint, String> {
    if let Ok(fp) = target.parse::<Fingerprint>() {
        return Ok(fp);
    }
    let t = target.trim();
    let mut matches = devices.iter().filter(|d| d.device.id.short().eq_ignore_ascii_case(t) || d.device.name.eq_ignore_ascii_case(t));
    match (matches.next(), matches.next()) {
        (Some(one), None) => Ok(one.device.id),
        (None, _) => Err(format!("No device called \"{t}\". Run `linksy devices` to see what's nearby.")),
        _ => Err(format!("Several devices are called \"{t}\". Use the device id instead.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linksy_core::{DeviceInfo, DeviceKind};

    fn view(n: u8, name: &str) -> DeviceView {
        let device = DeviceInfo { id: Fingerprint([n; 32]), name: name.into(), kind: DeviceKind::Phone, os: "android".into(), features: vec![], away: None };
        serde_json::from_value(serde_json::json!({ "device": device, "trusted": true, "connected": true, "nearby": true })).unwrap()
    }

    #[test]
    fn a_name_a_short_id_and_a_padded_name_find_the_device() {
        let devices = [view(1, "Pixel"), view(2, "Tab")];
        let pixel = Fingerprint([1; 32]);
        assert_eq!(resolve_device(&devices, "pixel"), Ok(pixel));
        assert_eq!(resolve_device(&devices, "  Pixel \n"), Ok(pixel));
        assert_eq!(resolve_device(&devices, &pixel.short().to_lowercase()), Ok(pixel));
    }

    #[test]
    fn a_full_fingerprint_is_taken_as_is() {
        let devices = [view(1, "Pixel")];
        let other = Fingerprint([9; 32]);
        assert_eq!(resolve_device(&devices, &other.to_string()), Ok(other));
        assert_eq!(resolve_device(&devices, &Fingerprint([1; 32]).to_string()), Ok(Fingerprint([1; 32])));
    }

    #[test]
    fn nothing_or_several_is_an_error_that_says_so() {
        let devices = [view(1, "Pixel"), view(2, "pixel")];
        assert!(resolve_device(&devices, "Nokia").unwrap_err().starts_with("No device called \"Nokia\""));
        assert!(resolve_device(&devices, " PIXEL ").unwrap_err().starts_with("Several devices are called \"PIXEL\""));
    }
}
