//! Wi-Fi station and SNTP, used to obtain correct time.
//!
//! `EspWifi::new` is called without an NVS partition, so the driver keeps its
//! configuration in RAM and nothing about the network is written to flash. A
//! reboot drops the link and the driver's copy of the credentials; the settings
//! file on the SD card is what makes a re-join possible after a power cycle.
//! `CONFIG_ESP_PHY_CALIBRATION_AND_DATA_STORAGE=n` keeps IDF from caching RF
//! calibration data in NVS as well, so enabling the radio performs no flash
//! writes at all.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::console;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::sntp::{EspSntp, SntpConf, SyncStatus};
use esp_idf_svc::sys::{self, EspError};
use esp_idf_svc::wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi};

const CONNECT_TIMEOUT_MS: u64 = 15_000;
/// Association and DHCP are separate milestones: the link can come up and still
/// leave the device without a route, so the address gets its own bounded wait.
const IP_TIMEOUT_MS: u64 = 10_000;
const SNTP_TIMEOUT_MS: u64 = 20_000;
const POLL_MS: u64 = 100;
/// NTP servers are names, not arbitrary console text.
const MAX_SERVER_LEN: usize = 63;

/// Human-readable auth mode; unknown values stay distinguishable.
fn auth_name(method: Option<AuthMethod>) -> &'static str {
    match method {
        Some(AuthMethod::None) => "open",
        Some(AuthMethod::WEP) => "wep",
        Some(AuthMethod::WPA) => "wpa",
        Some(AuthMethod::WPA2Personal) => "wpa2",
        Some(AuthMethod::WPAWPA2Personal) => "wpa/wpa2",
        Some(AuthMethod::WPA2Enterprise) => "wpa2-ent",
        Some(AuthMethod::WPA3Personal) => "wpa3",
        Some(AuthMethod::WPA2WPA3Personal) => "wpa2/wpa3",
        Some(AuthMethod::WAPIPersonal) => "wapi",
        None => "unknown",
    }
}

pub struct Net<'d> {
    wifi: EspWifi<'d>,
    sntp: Option<EspSntp<'d>>,
    started: bool,
    /// `esp-idf-svc` only selects station mode inside `set_configuration`, and the
    /// driver starts in `WIFI_MODE_NULL`, where scanning fails outright.
    /// Scanning therefore installs an empty station config first.
    configured: bool,
}

impl<'d> Net<'d> {
    pub fn new(modem: Modem<'d>, sys_loop: EspSystemEventLoop) -> Result<Self, String> {
        // `None` disables NVS-backed Wi-Fi storage: credentials are never written.
        let wifi = EspWifi::new(modem, sys_loop, None)
            .map_err(|error| format!("ERR wifi init: {error}\r\n"))?;
        Ok(Self {
            wifi,
            sntp: None,
            started: false,
            configured: false,
        })
    }

    fn start_if_needed(&mut self) -> Result<(), String> {
        if !self.started {
            self.wifi
                .start()
                .map_err(|error| format!("ERR wifi start: {error}\r\n"))?;
            self.started = true;
        }
        Ok(())
    }

    /// Select station mode, which a scan needs. `esp-idf-svc` only does this as a
    /// side effect of `set_configuration`, and installing an empty SSID just to
    /// scan would be a lie about the driver state, so the mode is set directly.
    fn ensure_station_mode(&mut self) -> Result<(), String> {
        if !self.configured {
            EspError::convert(unsafe { sys::esp_wifi_set_mode(sys::wifi_mode_t_WIFI_MODE_STA) })
                .map_err(|error| format!("ERR wifi station mode: {error}\r\n"))?;
            self.configured = true;
        }
        Ok(())
    }

    fn ensure_started(&mut self) -> Result<(), String> {
        self.ensure_station_mode()?;
        self.start_if_needed()
    }

    /// Wait until DHCP has handed the station an address. `is_up` is true only
    /// when the interface is up *and* holds a non-zero IPv4 address, which is
    /// exactly the condition SNTP needs.
    fn wait_for_ip(&mut self) -> Result<(), String> {
        let deadline = Duration::from_millis(IP_TIMEOUT_MS);
        let step = Duration::from_millis(POLL_MS);
        let mut waited = Duration::ZERO;
        while waited < deadline {
            if let Ok(true) = self.wifi.sta_netif().is_up() {
                return Ok(());
            }
            std::thread::sleep(step);
            waited += step;
        }
        Err(format!(
            "ERR wifi associated but no IP address after {} ms\r\n",
            deadline.as_millis()
        ))
    }

    pub fn scan(&mut self) -> Result<String, String> {
        self.ensure_started()?;
        let found = self
            .wifi
            .scan()
            .map_err(|error| format!("ERR wifi scan: {error}\r\n"))?;
        let mut reply = format!("OK wifi scan count={}\r\n", found.len());
        for access_point in found.iter().filter(|ap| !ap.ssid.is_empty()) {
            reply.push_str(&format!(
                "  ssid={} rssi={} channel={} auth={}\r\n",
                access_point.ssid,
                access_point.signal_strength,
                access_point.channel,
                auth_name(access_point.auth_method)
            ));
        }
        Ok(reply)
    }

    /// Join a network. The reply never contains the password.
    pub fn join(&mut self, ssid: &str, password: &str) -> Result<String, String> {
        if let Some(error) = console::credential_error(ssid, password) {
            return Err(format!("{error}\r\n"));
        }
        let config = ClientConfiguration {
            ssid: ssid
                .try_into()
                .map_err(|_| "ERR wifi ssid is not a valid SSID string\r\n".to_owned())?,
            password: password
                .try_into()
                .map_err(|_| "ERR wifi password is too long\r\n".to_owned())?,
            ..Default::default()
        };
        self.wifi
            .set_configuration(&Configuration::Client(config))
            .map_err(|error| format!("ERR wifi configuration: {error}\r\n"))?;
        self.configured = true;
        self.start_if_needed()?;
        if self
            .wifi
            .is_connected()
            .map_err(|error| format!("ERR wifi state: {error}\r\n"))?
        {
            self.wifi
                .disconnect()
                .map_err(|error| format!("ERR wifi disconnect: {error}\r\n"))?;
        }
        self.wifi
            .connect()
            .map_err(|error| format!("ERR wifi connect: {error}\r\n"))?;

        // Bounded wait: never block the console on an access point that is absent.
        let deadline = Duration::from_millis(CONNECT_TIMEOUT_MS);
        let step = Duration::from_millis(POLL_MS);
        let mut waited = Duration::ZERO;
        while waited < deadline {
            if let Ok(true) = self.wifi.is_connected() {
                // Association alone is not a usable network; wait for DHCP.
                self.wait_for_ip()?;
                return self.status();
            }
            std::thread::sleep(step);
            waited += step;
        }
        let _ = self.wifi.disconnect();
        Err(format!(
            "ERR wifi connect timed out after {} ms\r\n",
            deadline.as_millis()
        ))
    }

    /// Whether the station currently holds a link, so callers can decide whether
    /// stored credentials need to be used.
    pub fn is_connected(&self) -> Result<bool, String> {
        self.wifi
            .is_connected()
            .map_err(|error| format!("ERR wifi state: {error}\r\n"))
    }

    /// The station's IPv4 address and MAC, once DHCP has given it an address.
    /// The MAC is the chip's base MAC, which the station interface uses.
    pub fn address(&self) -> Option<(std::net::Ipv4Addr, [u8; 6])> {
        let netif = self.wifi.sta_netif();
        if !netif.is_up().unwrap_or(false) {
            return None;
        }
        let ip = netif.get_ip_info().ok()?.ip;
        Some((ip, netif.get_mac().ok()?))
    }

    /// Keep the radio awake (`false`) or let it doze between beacons (`true`,
    /// IDF's default modem sleep). Dozing, the frame missed nearly every
    /// multicast mDNS query while idle (0 of 12; 9 of 12 while being pinged),
    /// so discovery needs it awake while the servers run.
    pub fn set_power_save(&mut self, on: bool) -> Result<(), String> {
        let mode = if on {
            sys::wifi_ps_type_t_WIFI_PS_MIN_MODEM
        } else {
            sys::wifi_ps_type_t_WIFI_PS_NONE
        };
        EspError::convert(unsafe { sys::esp_wifi_set_ps(mode) })
            .map_err(|error| format!("ERR wifi power save: {error}\r\n"))
    }

    pub fn status(&mut self) -> Result<String, String> {
        let connected = self
            .wifi
            .is_connected()
            .map_err(|error| format!("ERR wifi state: {error}\r\n"))?;
        if !self.wifi.sta_netif().is_up().unwrap_or(false) {
            // Report the missing address instead of printing 0.0.0.0 as if it
            // were an assigned one.
            return Ok(format!(
                "OK wifi=<{}> ip=<none> started={} sntp={}\r\n",
                if connected {
                    "associated, awaiting dhcp"
                } else {
                    "not connected"
                },
                self.started,
                self.sntp.is_some()
            ));
        }
        let info = self
            .wifi
            .sta_netif()
            .get_ip_info()
            .map_err(|error| format!("ERR wifi netif: {error}\r\n"))?;
        let dns = self.wifi.sta_netif().get_dns();
        Ok(format!(
            "OK wifi=connected ip={} netmask={} gateway={} dns={dns}\r\n",
            info.ip, info.subnet.mask, info.subnet.gateway
        ))
    }

    /// Start SNTP once, then wait for the first synchronisation and return the
    /// resulting Unix time.
    pub fn sync_time(&mut self, server: Option<&str>) -> Result<u64, String> {
        if !self
            .wifi
            .is_connected()
            .map_err(|error| format!("ERR wifi state: {error}\r\n"))?
        {
            return Err("ERR wifi is not connected; use wifi join first\r\n".to_owned());
        }
        if let Some(name) = server
            && name.len() > MAX_SERVER_LEN
        {
            return Err(format!(
                "ERR wifi ntp server must be at most {MAX_SERVER_LEN} bytes\r\n"
            ));
        }
        if self.sntp.is_none() {
            let mut conf = SntpConf::default();
            if let Some(name) = server {
                conf.servers[0] = name;
            }
            self.sntp =
                Some(EspSntp::new(&conf).map_err(|error| format!("ERR sntp start: {error}\r\n"))?);
        }
        let deadline = Duration::from_millis(SNTP_TIMEOUT_MS);
        let step = Duration::from_millis(POLL_MS);
        let mut waited = Duration::ZERO;
        while waited < deadline {
            if let Some(sntp) = self.sntp.as_ref()
                && matches!(sntp.get_sync_status(), SyncStatus::Completed)
            {
                return SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|delta| delta.as_secs())
                    .map_err(|error| format!("ERR system clock: {error}\r\n"));
            }
            std::thread::sleep(step);
            waited += step;
        }
        Err(format!(
            "ERR sntp no reply after {} ms\r\n",
            deadline.as_millis()
        ))
    }

    pub fn off(&mut self) -> Result<String, String> {
        let _ = self.wifi.disconnect();
        if self.started {
            self.wifi
                .stop()
                .map_err(|error| format!("ERR wifi stop: {error}\r\n"))?;
            self.started = false;
        }
        self.sntp = None;
        Ok("OK wifi off\r\n".to_owned())
    }
}
