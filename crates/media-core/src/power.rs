//! Power source, so heavy background work can pause on laptop battery.

/// True when the computer is running on battery (Windows). Always false elsewhere.
pub fn on_battery() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut st = SYSTEM_POWER_STATUS::default();
        // ACLineStatus: 0 = offline (battery), 1 = online, 255 = unknown.
        if unsafe { GetSystemPowerStatus(&mut st) }.is_ok() {
            return st.ACLineStatus == 0;
        }
    }
    false
}
