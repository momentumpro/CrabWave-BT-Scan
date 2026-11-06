use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use btleplug::api::{
    AddressType, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::Manager;
use eframe::egui;
use futures::StreamExt;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
enum OuiLookupState {
    Idle,
    Loading,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
struct DeviceRow {
    name: String,
    device_type: String,
    address: String,
    rssi: Option<i16>,
    manufacturer_ids: HashSet<u16>,
    service_uuids: HashSet<Uuid>,
    class: Option<u32>,
    address_type: Option<AddressType>,
    oui_vendor: Option<String>,
    oui_state: OuiLookupState,
}

impl DeviceRow {
    fn merge(&mut self, other: &DeviceRow) {
        if is_better_name(&other.name, &self.name) {
            self.name = other.name.clone();
        }
        if is_better_type(&other.device_type, &self.device_type) {
            self.device_type = other.device_type.clone();
        }
        if other.rssi.is_some() {
            self.rssi = other.rssi;
        }
        self.manufacturer_ids.extend(&other.manufacturer_ids);
        self.service_uuids.extend(&other.service_uuids);
        if other.class.is_some() {
            self.class = other.class;
        }
        if other.address_type.is_some() {
            self.address_type = other.address_type;
        }
        // Preserve OUI lookup results across advertisement merges.

        // Recompute type/name hints once we have more advertisement data.
        let refreshed = classify_device(
            named_or_none(&self.name),
            &self.manufacturer_ids,
            &self.service_uuids,
            self.class,
            self.address_type,
        );
        if is_better_name(&refreshed.name, &self.name) {
            self.name = refreshed.name;
        }
        if is_better_type(&refreshed.device_type, &self.device_type) {
            self.device_type = refreshed.device_type;
        }
    }
}

#[derive(Clone)]
enum UiEvent {
    Status(String),
    Device(DeviceRow),
    OuiResult {
        address: String,
        vendor: Result<String, String>,
    },
    ScanFinished,
    Error(String),
}

struct BluetoothScanApp {
    devices: HashMap<String, DeviceRow>,
    status: String,
    scanning: bool,
    /// If true, hide devices weaker than `strong_rssi_min` (less negative = stronger).
    only_strong: bool,
    strong_rssi_min: i16,
    event_rx: mpsc::UnboundedReceiver<UiEvent>,
    event_tx: mpsc::UnboundedSender<UiEvent>,
    runtime: Arc<tokio::runtime::Runtime>,
    stop_flag: Arc<Mutex<bool>>,
}

impl BluetoothScanApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to create tokio runtime"),
        );

        Self {
            devices: HashMap::new(),
            status: "Ready.".into(),
            scanning: false,
            only_strong: false,
            strong_rssi_min: -70,
            event_rx,
            event_tx,
            runtime,
            stop_flag: Arc::new(Mutex::new(false)),
        }
    }

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }

        self.scanning = true;
        self.devices.clear();
        self.status = "Scanning…".into();
        *self.stop_flag.lock().unwrap() = false;

        let tx = self.event_tx.clone();
        let stop_flag = Arc::clone(&self.stop_flag);

        self.runtime.spawn(async move {
            if let Err(err) = run_scan(tx.clone(), stop_flag).await {
                let _ = tx.send(UiEvent::Error(err));
            }
            let _ = tx.send(UiEvent::ScanFinished);
        });
    }

    fn stop_scan(&mut self) {
        *self.stop_flag.lock().unwrap() = true;
        self.status = "Stopping…".into();
    }

    fn lookup_oui(&mut self, address: String) {
        let Some(device) = self.devices.get_mut(&address) else {
            return;
        };
        if device.oui_state == OuiLookupState::Loading {
            return;
        }
        device.oui_state = OuiLookupState::Loading;
        device.oui_vendor = None;
        self.status = format!("Looking up OUI for {address}…");

        let tx = self.event_tx.clone();
        let mac = address.clone();
        self.runtime.spawn(async move {
            let result = fetch_oui_vendor(&mac).await;
            let _ = tx.send(UiEvent::OuiResult {
                address: mac,
                vendor: result,
            });
        });
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::Status(msg) => self.status = msg,
                UiEvent::Device(device) => {
                    self.devices
                        .entry(device.address.clone())
                        .and_modify(|existing| existing.merge(&device))
                        .or_insert(device);
                }
                UiEvent::OuiResult { address, vendor } => {
                    if let Some(device) = self.devices.get_mut(&address) {
                        match vendor {
                            Ok(name) => {
                                device.oui_vendor = Some(name.clone());
                                device.oui_state = OuiLookupState::Done;
                                self.status = format!("{address} → {name}");
                            }
                            Err(err) => {
                                device.oui_vendor = Some(err.clone());
                                device.oui_state = OuiLookupState::Failed;
                                self.status = format!("{address}: {err}");
                            }
                        }
                    }
                }
                UiEvent::ScanFinished => {
                    self.scanning = false;
                    if !self.status.starts_with("Error") && !self.status.contains("→") {
                        let named = self
                            .devices
                            .values()
                            .filter(|d| !d.name.starts_with("(unnamed"))
                            .count();
                        self.status = format!(
                            "Done — {} device(s), {} named.",
                            self.devices.len(),
                            named
                        );
                    }
                }
                UiEvent::Error(msg) => {
                    self.scanning = false;
                    self.status = format!("Error: {msg}");
                }
            }
        }
    }
}

async fn run_scan(
    tx: mpsc::UnboundedSender<UiEvent>,
    stop_flag: Arc<Mutex<bool>>,
) -> Result<(), String> {
    let manager = Manager::new()
        .await
        .map_err(|e| format!("Could not start the Bluetooth manager: {e}"))?;

    let adapters = manager
        .adapters()
        .await
        .map_err(|e| format!("Could not list Bluetooth adapters: {e}"))?;

    let adapter = adapters.into_iter().next().ok_or_else(|| {
        "No Bluetooth adapter found. Turn on system Bluetooth and try again.".to_string()
    })?;

    let info = adapter
        .adapter_info()
        .await
        .unwrap_or_else(|_| "local adapter".into());
    let _ = tx.send(UiEvent::Status(format!("Scanning… ({info})")));

    // Subscribe before start_scan so we don't miss early advertisements.
    let mut events = adapter
        .events()
        .await
        .map_err(|e| format!("Could not subscribe to adapter events: {e}"))?;

    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| format!("Could not start scan: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(18);

    loop {
        if *stop_flag.lock().unwrap() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }

        let next = tokio::time::timeout(Duration::from_millis(250), events.next()).await;
        let Ok(Some(event)) = next else {
            continue;
        };

        let id = match &event {
            CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => Some(id.clone()),
            CentralEvent::ManufacturerDataAdvertisement { id, .. }
            | CentralEvent::ServiceDataAdvertisement { id, .. }
            | CentralEvent::ServicesAdvertisement { id, .. } => Some(id.clone()),
            _ => None,
        };

        if let Some(id) = id {
            if let Ok(peripheral) = adapter.peripheral(&id).await {
                if let Some(row) = device_row_from_peripheral(&peripheral).await {
                    let _ = tx.send(UiEvent::Device(row));
                }
            }
        }
    }

    // Final pass: refresh all known peripherals in case names arrived late.
    if let Ok(peripherals) = adapter.peripherals().await {
        for peripheral in peripherals {
            if let Some(row) = device_row_from_peripheral(&peripheral).await {
                let _ = tx.send(UiEvent::Device(row));
            }
        }
    }

    let _ = adapter.stop_scan().await;
    Ok(())
}

async fn device_row_from_peripheral(
    peripheral: &btleplug::platform::Peripheral,
) -> Option<DeviceRow> {
    let props = peripheral.properties().await.ok().flatten()?;
    let address = peripheral.address().to_string();
    let manufacturer_ids: HashSet<u16> = props.manufacturer_data.keys().copied().collect();
    let service_uuids: HashSet<Uuid> = props
        .services
        .iter()
        .copied()
        .chain(props.service_data.keys().copied())
        .collect();

    let classified = classify_device(
        props.local_name.clone(),
        &manufacturer_ids,
        &service_uuids,
        props.class,
        props.address_type,
    );

    Some(DeviceRow {
        name: classified.name,
        device_type: classified.device_type,
        address,
        rssi: props.rssi,
        manufacturer_ids,
        service_uuids,
        class: props.class,
        address_type: props.address_type,
        oui_vendor: None,
        oui_state: OuiLookupState::Idle,
    })
}

#[derive(Debug, serde::Deserialize)]
struct MacLookupResponse {
    success: bool,
    found: Option<bool>,
    company: Option<String>,
    #[serde(rename = "isRand")]
    is_rand: Option<bool>,
    #[serde(rename = "macPrefix")]
    mac_prefix: Option<String>,
}

async fn fetch_oui_vendor(mac: &str) -> Result<String, String> {
    let normalized = mac.trim().replace('-', ":").to_uppercase();
    if normalized.len() < 8 {
        return Err("Invalid MAC address".into());
    }

    // First octet locally-administered bit => not an IEEE OUI.
    if let Some(first) = normalized.split(':').next() {
        if let Ok(octet) = u8::from_str_radix(first, 16) {
            if octet & 0x02 != 0 {
                return Err(
                    "Locally administered / random MAC: IEEE OUI does not apply".into(),
                );
            }
        }
    }

    let url = format!("https://api.maclookup.app/v2/macs/{normalized}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("crabwave/0.1")
        .build()
        .map_err(|e| format!("Could not create HTTP client: {e}"))?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("API HTTP {}", response.status()));
    }

    let payload: MacLookupResponse = response
        .json()
        .await
        .map_err(|e| format!("Invalid JSON response: {e}"))?;

    if !payload.success {
        return Err("API could not process the MAC address".into());
    }

    if payload.is_rand == Some(true) {
        return Err("API marks this MAC as random (no OUI vendor)".into());
    }

    if payload.found != Some(true) {
        return Err("OUI not found in IEEE / database".into());
    }

    let company = payload
        .company
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| "No company name in API response".to_string())?;

    Ok(match payload.mac_prefix {
        Some(prefix) if !prefix.is_empty() => format!("{company} (OUI {prefix})"),
        _ => company,
    })
}

struct Classified {
    name: String,
    device_type: String,
}

fn classify_device(
    local_name: Option<String>,
    manufacturer_ids: &HashSet<u16>,
    service_uuids: &HashSet<Uuid>,
    class: Option<u32>,
    address_type: Option<AddressType>,
) -> Classified {
    let company = manufacturer_ids.iter().find_map(|id| company_label(*id));
    let service_type = infer_type_from_services(service_uuids);
    let class_type = class.and_then(class_of_device_label);
    let apple_hint = manufacturer_ids
        .contains(&0x004C)
        .then_some("Apple Continuity / accessory");

    let device_type = service_type
        .or_else(|| class_type.map(str::to_string))
        .or_else(|| apple_hint.map(str::to_string))
        .or_else(|| company.map(|c| format!("Manufacturer: {c}")))
        .or_else(|| {
            address_type.map(|t| match t {
                AddressType::Public => "Public address".into(),
                AddressType::Random => "Random BLE address".into(),
            })
        })
        .unwrap_or_else(|| "Unknown".into());

    let name = match local_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(n) => n,
        None => match company {
            Some(c) => format!("(unnamed · {c})"),
            None => "(unnamed)".into(),
        },
    };

    Classified { name, device_type }
}

fn company_label(id: u16) -> Option<&'static str> {
    Some(match id {
        0x0006 => "Microsoft",
        0x000D => "Texas Instruments",
        0x000F => "Broadcom",
        0x001D => "Qualcomm",
        0x004C => "Apple",
        0x0059 => "Nordic Semiconductor",
        0x0075 => "Samsung",
        0x00E0 => "Google",
        0x0157 => "Huami / Amazfit",
        0x02E5 => "Espressif",
        0x038F => "Xiaomi",
        0x0499 => "Ruuvi",
        0x0A7C => "Garmin",
        _ => return None,
    })
}

fn infer_type_from_services(services: &HashSet<Uuid>) -> Option<String> {
    // Bluetooth SIG 16-bit UUIDs expanded to 128-bit:
    // 0000XXXX-0000-1000-8000-00805f9b34fb
    let mut labels = Vec::new();

    for uuid in services {
        if let Some(label) = service_label(uuid) {
            if !labels.iter().any(|l| l == &label) {
                labels.push(label);
            }
        }
    }

    if labels.is_empty() {
        None
    } else {
        Some(labels.join(", "))
    }
}

fn service_label(uuid: &Uuid) -> Option<&'static str> {
    let short = uuid_to_16bit(uuid)?;
    Some(match short {
        0x1800 => "Generic Access",
        0x1801 => "Generic Attribute",
        0x180A => "Device Information",
        0x180F => "Battery",
        0x180D => "Heart Rate",
        0x1810 => "Blood Pressure",
        0x1812 => "HID (keyboard/mouse)",
        0x181C => "User Data",
        0x1819 => "Location and Navigation",
        0x1822 => "Pulse Oximeter",
        0x110A => "Audio (A2DP source)",
        0x110B => "Audio (A2DP sink)",
        0x110E => "AV Remote Control",
        0x111E => "Hands-Free",
        0x1200 => "PnP",
        0xFD6F => "Exposure Notification",
        0xFE2C => "Google Fast Pair",
        0xFE95 => "Xiaomi",
        0xFE9F => "Google",
        0xFFF0 => "Vendor service",
        _ => return None,
    })
}

fn uuid_to_16bit(uuid: &Uuid) -> Option<u16> {
    let bytes = uuid.as_bytes();
    // Bluetooth base UUID: 0000XXXX-0000-1000-8000-00805F9B34FB
    let base_tail: [u8; 12] = [
        0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB,
    ];
    if bytes[0] == 0x00 && bytes[1] == 0x00 && bytes[4..] == base_tail {
        return Some(u16::from_be_bytes([bytes[2], bytes[3]]));
    }
    None
}

fn class_of_device_label(class: u32) -> Option<&'static str> {
    // Major Device Class occupies bits 8-12.
    match (class >> 8) & 0x1F {
        0x01 => Some("Computer"),
        0x02 => Some("Phone"),
        0x03 => Some("Network / AP"),
        0x04 => Some("Audio / Video"),
        0x05 => Some("Peripheral (HID)"),
        0x06 => Some("Imaging"),
        0x07 => Some("Wearable"),
        0x08 => Some("Toy"),
        0x09 => Some("Health"),
        _ => None,
    }
}

fn named_or_none(name: &str) -> Option<String> {
    if name.starts_with("(unnamed") {
        None
    } else {
        Some(name.to_string())
    }
}

fn is_better_name(new: &str, old: &str) -> bool {
    score_name(new) > score_name(old)
}

fn is_better_type(new: &str, old: &str) -> bool {
    score_type(new) > score_type(old)
}

fn score_name(name: &str) -> u8 {
    if name.is_empty() || name == "(unnamed)" {
        0
    } else if name.starts_with("(unnamed · ") {
        1
    } else {
        2
    }
}

fn score_type(device_type: &str) -> u8 {
    match device_type {
        "" | "Unknown" => 0,
        t if t.starts_with("Public address") || t.starts_with("Random BLE") => 1,
        t if t.starts_with("Manufacturer:") => 2,
        t if t.contains("Continuity") => 2,
        _ => 3,
    }
}

/// Rough proximity band from RSSI. Not meters — walls/body/antennas distort a lot.
fn proximity_label(rssi: Option<i16>) -> &'static str {
    match rssi {
        Some(v) if v >= -50 => "Very near",
        Some(v) if v >= -70 => "Near",
        Some(v) if v >= -85 => "Medium",
        Some(_) => "Far",
        None => "No RSSI",
    }
}

fn rssi_color(rssi: Option<i16>) -> egui::Color32 {
    match rssi {
        Some(v) if v >= -50 => egui::Color32::from_rgb(40, 160, 70),
        Some(v) if v >= -70 => egui::Color32::from_rgb(90, 140, 40),
        Some(v) if v >= -85 => egui::Color32::from_rgb(180, 140, 30),
        Some(_) => egui::Color32::from_rgb(160, 70, 70),
        None => egui::Color32::GRAY,
    }
}

impl eframe::App for BluetoothScanApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        let lookup_pending = self
            .devices
            .values()
            .any(|d| d.oui_state == OuiLookupState::Loading);

        if self.scanning || lookup_pending {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let mut pending_lookup: Option<String> = None;
        let mut copied_mac: Option<String> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                paint_logo(ui, 28.0);
                ui.heading(
                    egui::RichText::new("CrabWave").strong().size(22.0),
                );
            });
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.scanning, egui::Button::new("Scan"))
                    .clicked()
                {
                    self.start_scan();
                }

                if ui
                    .add_enabled(self.scanning, egui::Button::new("Stop"))
                    .clicked()
                {
                    self.stop_scan();
                }

                if ui.button("Clear").clicked() {
                    self.devices.clear();
                }

                ui.separator();
                ui.checkbox(
                    &mut self.only_strong,
                    format!("Nearby (≥ {} dBm)", self.strong_rssi_min),
                );
            });

            ui.add_space(6.0);
            ui.label(&self.status);
            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("device_grid")
                        .striped(true)
                        .num_columns(9)
                        .spacing([12.0, 6.0])
                        .min_col_width(70.0)
                        .show(ui, |ui| {
                            ui.strong("#");
                            ui.strong("Name");
                            ui.strong("Type");
                            ui.strong("Proximity");
                            ui.strong("MAC");
                            ui.strong("");
                            ui.strong("RSSI");
                            ui.strong("Vendor");
                            ui.strong("");
                            ui.end_row();

                            let mut rows: Vec<_> = self.devices.values().cloned().collect();
                            rows.sort_by(|a, b| {
                                b.rssi
                                    .unwrap_or(i16::MIN)
                                    .cmp(&a.rssi.unwrap_or(i16::MIN))
                                    .then_with(|| a.name.cmp(&b.name))
                            });

                            if self.only_strong {
                                rows.retain(|d| {
                                    d.rssi
                                        .map(|v| v >= self.strong_rssi_min)
                                        .unwrap_or(false)
                                });
                            }

                            for (rank, device) in rows.into_iter().enumerate() {
                                ui.label(format!("{}", rank + 1));
                                ui.label(&device.name);
                                ui.label(&device.device_type);

                                let prox = proximity_label(device.rssi);
                                ui.colored_label(rssi_color(device.rssi), prox);

                                let mac_response = ui
                                    .add(
                                        egui::Label::new(
                                            egui::RichText::new(&device.address).monospace(),
                                        )
                                        .sense(egui::Sense::click()),
                                    )
                                    .on_hover_text("Copy MAC");
                                if mac_response.clicked() {
                                    copied_mac = Some(device.address.clone());
                                }

                                if ui.small_button("Copy").clicked() {
                                    copied_mac = Some(device.address.clone());
                                }

                                ui.colored_label(
                                    rssi_color(device.rssi),
                                    device
                                        .rssi
                                        .map(|v| format!("{v} dBm"))
                                        .unwrap_or_else(|| "—".into()),
                                );

                                let oui_text = match (&device.oui_state, &device.oui_vendor) {
                                    (OuiLookupState::Loading, _) => "…".into(),
                                    (_, Some(v)) => v.clone(),
                                    (OuiLookupState::Idle, None) => "—".into(),
                                    _ => "—".into(),
                                };
                                ui.label(oui_text);

                                let busy = device.oui_state == OuiLookupState::Loading;
                                let label = match device.oui_state {
                                    OuiLookupState::Done => "Refresh",
                                    OuiLookupState::Failed => "Retry",
                                    _ => "Vendor",
                                };
                                if ui
                                    .add_enabled(!busy, egui::Button::new(label))
                                    .clicked()
                                {
                                    pending_lookup = Some(device.address.clone());
                                }
                                ui.end_row();
                            }
                        });
                });
        });

        if let Some(address) = copied_mac {
            ctx.copy_text(address.clone());
            self.status = format!("MAC copied: {address}");
        }

        if let Some(address) = pending_lookup {
            self.lookup_oui(address);
        }
    }
}

fn paint_logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    let r = size * 0.42;

    // Soft blue “wave” disc
    painter.circle_filled(center, r, egui::Color32::from_rgb(36, 120, 190));
    painter.circle_stroke(
        center,
        r,
        egui::Stroke::new(1.2_f32, egui::Color32::from_rgb(20, 80, 140)),
    );

    // Orange crab body
    let crab = egui::Color32::from_rgb(230, 110, 45);
    painter.circle_filled(center + egui::vec2(0.0, 1.0), r * 0.38, crab);

    // Claws
    painter.circle_filled(center + egui::vec2(-r * 0.55, -r * 0.15), r * 0.16, crab);
    painter.circle_filled(center + egui::vec2(r * 0.55, -r * 0.15), r * 0.16, crab);

    // Eyes
    let eye = egui::Color32::from_rgb(30, 30, 30);
    painter.circle_filled(center + egui::vec2(-r * 0.14, -r * 0.08), r * 0.07, eye);
    painter.circle_filled(center + egui::vec2(r * 0.14, -r * 0.08), r * 0.07, eye);

    // Wave arcs
    let wave = egui::Color32::from_rgb(180, 220, 255);
    for (dy, rx) in [(-0.55, 0.55), (0.62, 0.62)] {
        painter.line_segment(
            [
                center + egui::vec2(-r * rx * 0.7, r * dy),
                center + egui::vec2(0.0, r * dy + r * 0.08),
            ],
            egui::Stroke::new(1.4_f32, wave),
        );
        painter.line_segment(
            [
                center + egui::vec2(0.0, r * dy + r * 0.08),
                center + egui::vec2(r * rx * 0.7, r * dy),
            ],
            egui::Stroke::new(1.4_f32, wave),
        );
    }
}

fn app_icon() -> egui::IconData {
    const SIZE: usize = 48;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];

    let put = |rgba: &mut [u8], x: i32, y: i32, c: [u8; 4]| {
        if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
            return;
        }
        let i = ((y as usize) * SIZE + x as usize) * 4;
        rgba[i..i + 4].copy_from_slice(&c);
    };

    let cx = (SIZE / 2) as f32;
    let cy = (SIZE / 2) as f32;
    let blue = [36u8, 120, 190, 255];
    let crab = [230u8, 110, 45, 255];
    let eye = [30u8, 30, 30, 255];
    let wave = [180u8, 220, 255, 255];

    for y in 0..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();

            if d <= 22.0 {
                put(&mut rgba, x, y, blue);
            }
            // crab body
            if (dx * dx + (dy - 1.0) * (dy - 1.0)).sqrt() <= 9.0 {
                put(&mut rgba, x, y, crab);
            }
            // claws
            if ((dx + 12.0).powi(2) + (dy + 2.0).powi(2)).sqrt() <= 4.0 {
                put(&mut rgba, x, y, crab);
            }
            if ((dx - 12.0).powi(2) + (dy + 2.0).powi(2)).sqrt() <= 4.0 {
                put(&mut rgba, x, y, crab);
            }
            // eyes
            if ((dx + 3.5).powi(2) + (dy + 1.0).powi(2)).sqrt() <= 1.6 {
                put(&mut rgba, x, y, eye);
            }
            if ((dx - 3.5).powi(2) + (dy + 1.0).powi(2)).sqrt() <= 1.6 {
                put(&mut rgba, x, y, eye);
            }
            // wave dashes
            if (dy - 12.0).abs() < 1.2 && dx.abs() < 14.0 && (dx as i32).abs() % 5 < 3 {
                put(&mut rgba, x, y, wave);
            }
            if (dy + 14.0).abs() < 1.2 && dx.abs() < 16.0 && (dx as i32).abs() % 5 < 3 {
                put(&mut rgba, x, y, wave);
            }
        }
    }

    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 560.0])
            .with_title("CrabWave")
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "CrabWave",
        options,
        Box::new(|cc| Ok(Box::new(BluetoothScanApp::new(cc)))),
    )
}
