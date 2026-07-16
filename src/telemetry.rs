use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Default)]
pub struct ComponentInfo {
    pub name: String,
    pub details: String,
}

#[derive(Clone, Debug, Default)]
pub struct SensorReading {
    pub kind: String,
    pub name: String,
    pub value: f64,
    pub unit: String,
}

#[derive(Clone, Debug, Default)]
pub struct GpuInfo {
    pub name: String,
    pub board_name: Option<String>,
    bus_id: Option<String>,
    pub usage_percent: Option<f64>,
    pub temperature_c: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub power_watts: Option<f64>,
    pub fan_percent: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct BatteryInfo {
    pub name: String,
    pub status: String,
    pub capacity_percent: Option<f64>,
    pub power_watts: Option<f64>,
    pub energy_full_wh: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    pub key: String,
    pub name: String,
    pub kind: String,
    pub transport: String,
    pub battery_percent: Option<f64>,
}

#[derive(Clone, Debug)]
struct DeviceCandidate {
    info: DeviceInfo,
    kind_confidence: u8,
}

#[derive(Clone, Debug, Default)]
pub struct DiskUsage {
    pub name: String,
    pub device: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub temperature_c: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct StaticInfo {
    pub architecture: String,
    pub system: String,
    pub cpu_model: String,
    pub cpu_topology: String,
    pub motherboard: String,
    pub bios: String,
    pub graphics: Vec<ComponentInfo>,
    pub storage: Vec<ComponentInfo>,
    pub pci_devices: Vec<ComponentInfo>,
    pub usb_devices: Vec<ComponentInfo>,
    pub batteries: Vec<ComponentInfo>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cpu_usage_percent: f64,
    pub cpu_temp_c: Option<f64>,
    pub memory_temp_c: Option<f64>,
    pub motherboard_temp_c: Option<f64>,
    pub cpu_frequency_mhz: Option<f64>,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub memory_used_bytes: u64,
    pub gpus: Vec<GpuInfo>,
    pub temperatures: Vec<SensorReading>,
    pub fans: Vec<SensorReading>,
    pub power: Vec<SensorReading>,
    pub batteries: Vec<BatteryInfo>,
    pub devices: Vec<DeviceInfo>,
    pub disks: Vec<DiskUsage>,
    pub static_info: StaticInfo,
}

#[derive(Clone, Debug, Default)]
struct GpuDescriptor {
    name: String,
    board_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
struct CpuTimes {
    idle: u64,
    total: u64,
}

struct HostRunner {
    in_flatpak: bool,
}

impl HostRunner {
    fn new() -> Self {
        Self {
            in_flatpak: env::var_os("FLATPAK_ID").is_some(),
        }
    }

    fn run_script(&self, script: &str) -> String {
        let output = if self.in_flatpak {
            Command::new("flatpak-spawn")
                .args(["--host", "sh", "-c", script])
                .output()
        } else {
            Command::new("sh").args(["-c", script]).output()
        };

        output
            .ok()
            .filter(|result| result.status.success())
            .map(|result| String::from_utf8_lossy(&result.stdout).into_owned())
            .unwrap_or_default()
    }
}

pub struct TelemetryCollector {
    runner: HostRunner,
    static_info: StaticInfo,
    gpu_descriptors: HashMap<String, GpuDescriptor>,
    previous_cpu: Option<CpuTimes>,
    cached_devices: Vec<DeviceInfo>,
    device_refresh_countdown: u8,
}

impl TelemetryCollector {
    pub fn new() -> Self {
        let runner = HostRunner::new();
        let (static_info, gpu_descriptors) = collect_static_info(&runner);
        Self {
            runner,
            static_info,
            gpu_descriptors,
            previous_cpu: None,
            cached_devices: Vec::new(),
            device_refresh_countdown: 0,
        }
    }

    pub fn collect(&mut self) -> Snapshot {
        let sections = split_sections(&self.runner.run_script(DYNAMIC_SCRIPT));
        let current_cpu = parse_cpu_times(section(&sections, "STAT"));
        let cpu_usage_percent = match (self.previous_cpu, current_cpu) {
            (Some(previous), Some(current)) if current.total > previous.total => {
                let total_delta = current.total - previous.total;
                let idle_delta = current.idle.saturating_sub(previous.idle);
                100.0 * (total_delta.saturating_sub(idle_delta)) as f64 / total_delta as f64
            }
            _ => 0.0,
        };
        self.previous_cpu = current_cpu;

        let memory = parse_meminfo(section(&sections, "MEMINFO"));

        let mut all_sensors = collect_hwmon_sensors();
        all_sensors.extend(collect_thermal_zone_sensors());
        all_sensors.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.name.cmp(&b.name))
        });
        all_sensors.dedup_by(|a, b| {
            a.kind == b.kind
                && a.name == b.name
                && (a.value - b.value).abs() < 0.5
        });
        let temperatures = all_sensors
            .iter()
            .filter(|sensor| sensor.unit == "°C")
            .cloned()
            .collect::<Vec<_>>();
        let fans = all_sensors
            .iter()
            .filter(|sensor| sensor.unit == "RPM")
            .cloned()
            .collect::<Vec<_>>();
        let power = all_sensors
            .iter()
            .filter(|sensor| sensor.unit == "W")
            .cloned()
            .collect::<Vec<_>>();
        let cpu_temp_c = temperatures
            .iter()
            .filter(|sensor| sensor.kind == "CPU temperature")
            .map(|sensor| sensor.value)
            .reduce(f64::max);
        let memory_temp_c = temperatures
            .iter()
            .filter(|sensor| sensor.kind == "Memory temperature")
            .map(|sensor| sensor.value)
            .reduce(f64::max);
        let motherboard_temp_c = temperatures
            .iter()
            .filter(|sensor| sensor.kind == "Motherboard temperature")
            .map(|sensor| sensor.value)
            .reduce(f64::max);

        let mut nvidia_gpus = parse_nvidia_gpus(section(&sections, "NVIDIA"));
        for gpu in &mut nvidia_gpus {
            let Some(bus_id) = gpu.bus_id.as_deref() else {
                continue;
            };
            if let Some(descriptor) = self.gpu_descriptors.get(bus_id) {
                gpu.board_name = descriptor.board_name.clone();
                if gpu.name.trim().is_empty() {
                    gpu.name = descriptor.name.clone();
                }
            }
        }
        let gpus = collect_sysfs_gpus(&self.gpu_descriptors, &nvidia_gpus);

        let mut disks = parse_disk_usage(section(&sections, "DISKS"));
        for disk in &mut disks {
            let root = disk.device.trim_start_matches("/dev/");
            disk.temperature_c = collect_disk_temperature(root);
        }

        if self.device_refresh_countdown == 0 {
            self.cached_devices = collect_connected_devices(&self.runner);
            self.device_refresh_countdown = 9;
        } else {
            self.device_refresh_countdown -= 1;
        }

        Snapshot {
            cpu_usage_percent,
            cpu_temp_c,
            memory_temp_c,
            motherboard_temp_c,
            cpu_frequency_mhz: collect_cpu_frequency_mhz(),
            memory_total_bytes: memory.total,
            memory_available_bytes: memory.available,
            memory_used_bytes: memory.total.saturating_sub(memory.available),
            gpus,
            temperatures,
            fans,
            power,
            batteries: collect_batteries(),
            devices: self.cached_devices.clone(),
            disks,
            static_info: self.static_info.clone(),
        }
    }
}

const DYNAMIC_SCRIPT: &str = r#"
export LC_ALL=C
printf '@@MEMINFO@@\n'
cat /proc/meminfo 2>/dev/null || true
printf '@@STAT@@\n'
head -n 1 /proc/stat 2>/dev/null || true
printf '@@NVIDIA@@\n'
if command -v nvidia-smi >/dev/null 2>&1; then
  nvidia-smi --query-gpu=pci.bus_id,name,utilization.gpu,temperature.gpu,memory.used,memory.total,power.draw,fan.speed --format=csv,noheader,nounits 2>/dev/null || true
fi
printf '@@DISKS@@\n'
if command -v df >/dev/null 2>&1 && command -v lsblk >/dev/null 2>&1; then
  df -B1 -PT -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs 2>/dev/null |
    awk 'NR > 1 { print $1 "\t" $2 "\t" $3 "\t" $4 "\t" $5 "\t" $7 }' |
    while IFS="$(printf '\t')" read -r source filesystem total used available mount_point; do
      case "$source" in
        /dev/*) ;;
        *) continue ;;
      esac
      root="$(lsblk -sno KNAME,TYPE "$source" 2>/dev/null | awk '$2 == "disk" { print $1; exit }')"
      if [ -z "$root" ]; then
        root="$(lsblk -no KNAME "$source" 2>/dev/null | head -n 1)"
      fi
      [ -n "$root" ] || continue
      model="$(lsblk -dn -o MODEL "/dev/$root" 2>/dev/null | sed 's/[[:space:]]*$//')"
      vendor="$(lsblk -dn -o VENDOR "/dev/$root" 2>/dev/null | sed 's/[[:space:]]*$//')"
      drive_size="$(lsblk -bdn -o SIZE "/dev/$root" 2>/dev/null | head -n 1)"
      [ -n "$drive_size" ] || drive_size="$total"
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$root" "$vendor" "$model" "$drive_size" "$source" "$filesystem" "$total" "$used" "$available"
    done
fi
"#;

const DEVICE_SCRIPT: &str = r#"
export LC_ALL=C

property_value() {
  printf '%s\n' "$1" | sed -n "s/^$2=//p" | head -n 1
}

clean_name() {
  printf '%s' "$1" | tr '_' ' ' | sed 's/[[:space:]][[:space:]]*/ /g; s/^ //; s/ $//'
}

usb_parent_for() {
  path="$(readlink -f "$1" 2>/dev/null)"
  while [ -n "$path" ] && [ "$path" != '/' ]; do
    if [ -r "$path/idVendor" ] && [ -r "$path/idProduct" ]; then
      printf '%s' "$path"
      return
    fi
    path="$(dirname "$path")"
  done
}

physical_id_for() {
  event="$1"
  props="$2"

  # Prefer the physical USB device identity over per-interface udev serials.
  # This merges a receiver's keyboard/mouse/control interfaces and lets its
  # vendor/product ID participate in wireless receiver detection.
  usb_parent="$(usb_parent_for "$event/device")"
  if [ -n "$usb_parent" ]; then
    vendor="$(cat "$usb_parent/idVendor" 2>/dev/null)"
    product="$(cat "$usb_parent/idProduct" 2>/dev/null)"
    usb_serial="$(cat "$usb_parent/serial" 2>/dev/null)"
    printf 'usb:%s:%s:%s:%s' "$vendor" "$product" "$usb_serial" "$(basename "$usb_parent")"
    return
  fi

  serial="$(property_value "$props" ID_SERIAL)"
  [ -n "$serial" ] || serial="$(property_value "$props" HID_UNIQ)"
  if [ -n "$serial" ]; then
    printf '%s' "$serial"
    return
  fi

  path_id="$(property_value "$props" ID_PATH)"
  [ -n "$path_id" ] || path_id="$(property_value "$props" DEVPATH)"
  [ -n "$path_id" ] || path_id="$(readlink -f "$event/device" 2>/dev/null)"
  printf '%s' "$path_id"
}

usb_receiver_hint_for() {
  usb_parent="$1"
  kind="$2"
  [ -n "$usb_parent" ] || return
  [ "$kind" = 'Mouse' ] || return

  interfaces="$(cat "$usb_parent/bNumInterfaces" 2>/dev/null)"
  case "$interfaces" in
    ''|*[!0-9]*) return ;;
  esac

  # Many 2.4 GHz mouse receivers expose several HID interfaces (mouse,
  # consumer controls, and sometimes a keyboard interface). This is a
  # receiver fallback after explicit USB names and known receiver IDs.
  if [ "$interfaces" -ge 2 ]; then
    printf 'receiver-topology'
  fi
}

transport_for() {
  bus="$1"
  name_lower="$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')"
  case "$bus" in
    bluetooth) printf 'Bluetooth' ;;
    usb)
      case "$name_lower" in
        *receiver*|*wireless*|*unifying*|*lightspeed*|*bolt*|*dongle*|*2.4g*|*2.4\ ghz*|*cordless*|*superlight*|*nano\ transceiver*|*usb\ transceiver*|*receiver-topology*) printf '2.4 GHz wireless' ;;
        *046d:c5??*) printf '2.4 GHz wireless' ;;
        *) printf 'Wired USB' ;;
      esac
      ;;
    i8042|serio|platform) printf 'Built-in' ;;
    *)
      case "$name_lower" in
        *receiver*|*wireless*|*dongle*|*2.4g*|*2.4\ ghz*|*cordless*|*superlight*) printf '2.4 GHz wireless' ;;
        *) printf 'Wired' ;;
      esac
      ;;
  esac
}

# Keyboards, mice, and game controllers exposed by udev.
if command -v udevadm >/dev/null 2>&1; then
  for event in /sys/class/input/event*; do
    [ -e "$event" ] || continue
    props="$(udevadm info --query=property --path="$event" 2>/dev/null)"
    [ -n "$props" ] || continue

    if printf '%s\n' "$props" | grep -q '^ID_INPUT_TOUCHPAD=1$'; then
      continue
    fi
    is_joystick=0
    is_keyboard=0
    is_mouse=0
    printf '%s\n' "$props" | grep -q '^ID_INPUT_JOYSTICK=1$' && is_joystick=1
    printf '%s\n' "$props" | grep -q '^ID_INPUT_KEYBOARD=1$' && is_keyboard=1
    printf '%s\n' "$props" | grep -q '^ID_INPUT_MOUSE=1$' && is_mouse=1
    [ "$is_joystick" -eq 1 ] || [ "$is_keyboard" -eq 1 ] || [ "$is_mouse" -eq 1 ] || continue

    name="$(property_value "$props" ID_MODEL_FROM_DATABASE)"
    [ -n "$name" ] || name="$(property_value "$props" ID_MODEL)"
    [ -n "$name" ] || name="$(property_value "$props" NAME)"
    [ -n "$name" ] || name="$(cat "$event/device/name" 2>/dev/null)"
    name="$(clean_name "$name")"
    [ -n "$name" ] || continue

    event_name="$(clean_name "$(cat "$event/device/name" 2>/dev/null)")"
    usb_parent="$(usb_parent_for "$event/device")"
    usb_description=''
    usb_identity=''
    if [ -n "$usb_parent" ]; then
      usb_description="$(clean_name "$(cat "$usb_parent/manufacturer" 2>/dev/null) $(cat "$usb_parent/product" 2>/dev/null)")"
      usb_identity="$(cat "$usb_parent/idVendor" 2>/dev/null):$(cat "$usb_parent/idProduct" 2>/dev/null)"
    fi
    udev_usb_description="$(clean_name "$(property_value "$props" ID_USB_VENDOR) $(property_value "$props" ID_USB_MODEL_FROM_DATABASE) $(property_value "$props" ID_USB_MODEL)")"

    # Classify the physical product before considering its auxiliary event
    # interfaces. A gaming mouse often exposes a keyboard-like macro interface;
    # a keyboard may expose a pointer interface for a wheel or touch surface.
    # Product-level names are authoritative. When those are generic, a keyboard
    # LED map identifies a real keyboard, while a true pointer event identifies
    # a mouse. Interface names alone never promote a mouse to a keyboard.
    product_label="$(clean_name "$usb_description $udev_usb_description")"
    [ -n "$product_label" ] || product_label="$name"
    product_lower="$(printf '%s' "$product_label" | tr '[:upper:]' '[:lower:]')"
    interface_lower="$(printf '%s' "$event_name" | tr '[:upper:]' '[:lower:]')"
    product_keyboard=0
    product_mouse=0
    interface_mouse=0
    case "$product_lower" in *keyboard*|*keypad*|*kbd*) product_keyboard=1 ;; esac
    case "$product_lower" in *mouse*|*trackball*) product_mouse=1 ;; esac
    case "$interface_lower" in *mouse*|*trackball*) interface_mouse=1 ;; esac
    led_caps="$(cat "$event/device/capabilities/led" 2>/dev/null | tr -d ' 0\n')"

    if [ "$is_joystick" -eq 1 ]; then
      kind='Controller'
      kind_confidence=250
    elif [ "$product_mouse" -eq 1 ] && [ "$product_keyboard" -eq 0 ]; then
      kind='Mouse'
      kind_confidence=250
    elif [ "$product_keyboard" -eq 1 ] && [ "$product_mouse" -eq 0 ]; then
      kind='Keyboard'
      kind_confidence=250
    elif [ "$is_keyboard" -eq 1 ] && [ -n "$led_caps" ]; then
      kind='Keyboard'
      kind_confidence=235
    elif [ "$is_mouse" -eq 1 ] && [ "$interface_mouse" -eq 1 ]; then
      kind='Mouse'
      kind_confidence=225
    elif [ "$is_mouse" -eq 1 ]; then
      kind='Mouse'
      kind_confidence=210
    elif [ "$is_keyboard" -eq 1 ]; then
      kind='Keyboard'
      kind_confidence=150
    else
      continue
    fi

    bus="$(property_value "$props" ID_BUS)"
    physical_id="$(physical_id_for "$event" "$props")"
    [ -n "$physical_id" ] || physical_id="$name"
    receiver_hint="$(usb_receiver_hint_for "$usb_parent" "$kind")"
    transport="$(transport_for "$bus" "$name $usb_description $udev_usb_description $usb_identity $receiver_hint")"
    printf 'D\t%s\t%s\t%s\t%s\t\t%s\n' "$kind" "$physical_id" "$name" "$transport" "$kind_confidence"
  done

  # USB and wireless headsets represented as sound cards.
  for card in /sys/class/sound/card*; do
    [ -e "$card" ] || continue
    props="$(udevadm info --query=property --path="$card" 2>/dev/null)"
    [ -n "$props" ] || continue
    name="$(property_value "$props" ID_MODEL_FROM_DATABASE)"
    [ -n "$name" ] || name="$(property_value "$props" ID_MODEL)"
    name="$(clean_name "$name")"
    [ -n "$name" ] || continue
    form="$(property_value "$props" SOUND_FORM_FACTOR)"
    lower="$(printf '%s %s' "$form" "$name" | tr '[:upper:]' '[:lower:]')"
    case "$lower" in
      *headset*|*headphone*|*earbud*|*airpod*|*arctis*|*blackshark*|*kraken*|*astro*|*hyperx\ cloud*|*g\ pro\ x*) ;;
      *) continue ;;
    esac
    bus="$(property_value "$props" ID_BUS)"
    serial="$(property_value "$props" ID_SERIAL)"
    [ -n "$serial" ] || serial="$name"
    transport="$(transport_for "$bus" "$name")"
    printf 'D\tHeadset\t%s\t%s\t%s\t\n' "$serial" "$name" "$transport"
  done
fi

# Bluetooth devices provide better names, categories, and sometimes battery data.
if command -v bluetoothctl >/dev/null 2>&1; then
  bluetoothctl devices 2>/dev/null | while read -r prefix address name; do
    [ "$prefix" = 'Device' ] || continue
    info="$(bluetoothctl info "$address" 2>/dev/null)"
    printf '%s\n' "$info" | grep -q 'Connected: yes' || continue
    icon="$(printf '%s\n' "$info" | sed -n 's/^[[:space:]]*Icon: //p' | head -n 1)"
    case "$icon" in
      input-keyboard) kind='Keyboard' ;;
      input-mouse) kind='Mouse' ;;
      input-gaming) kind='Controller' ;;
      audio-headset|audio-headphones|audio-card) kind='Headset' ;;
      *) continue ;;
    esac
    alias="$(printf '%s\n' "$info" | sed -n 's/^[[:space:]]*Alias: //p' | head -n 1)"
    [ -n "$alias" ] || alias="$name"
    battery="$(printf '%s\n' "$info" | sed -n 's/.*Battery Percentage:.*(\([0-9][0-9]*\)).*/\1/p' | head -n 1)"
    printf 'D\t%s\t%s\t%s\tBluetooth\t%s\t130\n' "$kind" "$address" "$alias" "$battery"
  done
fi

# Peripheral battery devices, including Logitech HID++ and many Bluetooth HID devices.
for supply in /sys/class/power_supply/*; do
  [ -e "$supply" ] || continue
  [ "$(cat "$supply/type" 2>/dev/null)" = 'Battery' ] || continue
  scope="$(cat "$supply/scope" 2>/dev/null)"
  base="$(basename "$supply")"
  case "$scope:$base" in
    Device:*|*:hidpp*|*:hid-*|*:bluetooth*) ;;
    *) continue ;;
  esac
  manufacturer="$(cat "$supply/manufacturer" 2>/dev/null)"
  model="$(cat "$supply/model_name" 2>/dev/null)"
  name="$(clean_name "$manufacturer $model")"
  [ -n "$name" ] || name="$base"
  capacity="$(cat "$supply/capacity" 2>/dev/null)"
  case "$capacity" in ''|*[!0-9.]*) continue ;; esac
  battery_usb_parent="$(usb_parent_for "$supply/device")"
  battery_identity=''
  if [ -n "$battery_usb_parent" ]; then
    battery_vendor="$(cat "$battery_usb_parent/idVendor" 2>/dev/null)"
    battery_product="$(cat "$battery_usb_parent/idProduct" 2>/dev/null)"
    battery_serial="$(cat "$battery_usb_parent/serial" 2>/dev/null)"
    battery_identity="usb:$battery_vendor:$battery_product:$battery_serial:$(basename "$battery_usb_parent")"
  fi
  printf 'B\t%s\t%s\t%s\n' "$name" "$capacity" "$battery_identity"
done
"#;

const STATIC_SCRIPT: &str = r#"
export LC_ALL=C
printf '@@CPUINFO@@\n'
cat /proc/cpuinfo 2>/dev/null || true
printf '@@LSCPU@@\n'
if command -v lscpu >/dev/null 2>&1; then lscpu 2>/dev/null || true; fi
printf '@@ARCH@@\n'
uname -m 2>/dev/null || true
printf '@@DEVICE_TREE_MODEL@@\n'
for path in /sys/firmware/devicetree/base/model /proc/device-tree/model; do
  if [ -r "$path" ]; then tr -d '\000' < "$path"; printf '\n'; break; fi
done
printf '@@DEVICE_TREE_COMPATIBLE@@\n'
for path in /sys/firmware/devicetree/base/compatible /proc/device-tree/compatible; do
  if [ -r "$path" ]; then tr '\000' '\n' < "$path"; break; fi
done
printf '@@FIRMWARE@@\n'
for path in \
  /sys/firmware/devicetree/base/firmware/version \
  /proc/device-tree/firmware/version \
  /sys/firmware/devicetree/base/chosen/bootloader-version \
  /proc/device-tree/chosen/bootloader-version; do
  if [ -r "$path" ]; then tr -d '\000' < "$path"; printf '\n'; break; fi
done
printf '@@LSPCI@@\n'
if command -v lspci >/dev/null 2>&1; then lspci -Dnn 2>/dev/null || true; fi
printf '@@LSPCI_VMM@@\n'
if command -v lspci >/dev/null 2>&1; then lspci -D -nn -vmm 2>/dev/null || true; fi
printf '@@LSBLK@@\n'
if command -v lsblk >/dev/null 2>&1; then lsblk -dn -P -o NAME,TYPE,SIZE,MODEL,ROTA,TRAN 2>/dev/null || true; fi
printf '@@LSUSB@@\n'
if command -v lsusb >/dev/null 2>&1; then lsusb 2>/dev/null || true; fi
"#;

fn collect_static_info(runner: &HostRunner) -> (StaticInfo, HashMap<String, GpuDescriptor>) {
    let sections = split_sections(&runner.run_script(STATIC_SCRIPT));
    let cpuinfo = section(&sections, "CPUINFO");
    let lscpu = section(&sections, "LSCPU");
    let architecture = section(&sections, "ARCH").trim().to_string();
    let cpu_model = collect_cpu_model(cpuinfo, lscpu, &architecture);
    let cpu_topology = collect_cpu_topology(cpuinfo, lscpu, &architecture);

    let device_tree_model = section(&sections, "DEVICE_TREE_MODEL").trim().to_string();
    let device_tree_compatible = section(&sections, "DEVICE_TREE_COMPATIBLE");
    let dmi_motherboard = join_non_empty(
        &[
            read_trim("/sys/class/dmi/id/board_vendor"),
            read_trim("/sys/class/dmi/id/board_name"),
            read_trim("/sys/class/dmi/id/board_version"),
        ],
        " ",
    );
    let motherboard = if dmi_motherboard.is_empty() {
        concise_device_tree_platform(device_tree_compatible)
    } else {
        dmi_motherboard
    };
    let dmi_bios = join_non_empty(
        &[
            read_trim("/sys/class/dmi/id/bios_vendor"),
            read_trim("/sys/class/dmi/id/bios_version"),
            read_trim("/sys/class/dmi/id/bios_date"),
        ],
        " ",
    );
    let bios = if dmi_bios.is_empty() {
        section(&sections, "FIRMWARE").trim().to_string()
    } else {
        dmi_bios
    };

    let dmi_system = join_non_empty(
        &[
            read_trim("/sys/class/dmi/id/sys_vendor"),
            read_trim("/sys/class/dmi/id/product_name"),
            read_trim("/sys/class/dmi/id/product_version"),
        ],
        " ",
    );
    let system = if dmi_system.is_empty() {
        device_tree_model
    } else {
        dmi_system
    };

    let lspci = section(&sections, "LSPCI");
    let gpu_descriptors = parse_gpu_descriptors(section(&sections, "LSPCI_VMM"));
    let (mut graphics, mut pci_devices) = parse_pci_inventory(lspci);
    for component in &mut graphics {
        let address = component.details.clone();
        if let Some(descriptor) = gpu_descriptors.get(&normalize_pci_address(&address)) {
            if !descriptor.name.trim().is_empty() {
                component.name = descriptor.name.clone();
            }
            if let Some(board_name) = descriptor.board_name.as_deref() {
                component.details = format!("{address} • {board_name}");
            }
        }
    }
    let mut storage = parse_lsblk(section(&sections, "LSBLK"));
    let mut usb_devices = parse_lsusb(section(&sections, "LSUSB"));
    let mut batteries = collect_battery_components();

    for devices in [&mut graphics, &mut storage, &mut pci_devices, &mut usb_devices, &mut batteries] {
        devices.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.details.cmp(&b.details)));
        devices.dedup_by(|a, b| a.name == b.name && a.details == b.details);
    }

    (
        StaticInfo {
            architecture,
            system,
            cpu_model,
            cpu_topology,
            motherboard,
            bios,
            graphics,
            storage,
            pci_devices,
            usb_devices,
            batteries,
        },
        gpu_descriptors,
    )
}

fn collect_cpu_model(cpuinfo: &str, lscpu: &str, architecture: &str) -> String {
    for key in ["Model name", "BIOS Model name"] {
        if let Some(value) = lscpu_value(lscpu, key).filter(|value| !value.trim().is_empty()) {
            return value;
        }
    }

    for key in ["model name", "Processor", "Hardware"] {
        if let Some(value) = cpuinfo
            .lines()
            .find_map(|line| value_after_colon(line, key))
            .filter(|value| !value.trim().is_empty())
        {
            return value;
        }
    }

    if architecture == "aarch64" || architecture.starts_with("arm") {
        let implementer = cpuinfo
            .lines()
            .find_map(|line| value_after_colon(line, "CPU implementer"));
        let part = cpuinfo
            .lines()
            .find_map(|line| value_after_colon(line, "CPU part"));
        return arm_cpu_fallback(implementer.as_deref(), part.as_deref());
    }

    if architecture.is_empty() {
        "Unknown processor".to_string()
    } else {
        format!("{} processor", architecture)
    }
}

fn collect_cpu_topology(cpuinfo: &str, lscpu: &str, architecture: &str) -> String {
    let logical = lscpu_value(lscpu, "CPU(s)")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| {
            cpuinfo
                .lines()
                .filter(|line| line.trim_start().starts_with("processor"))
                .count()
                .max(1)
        });
    let sockets = lscpu_value(lscpu, "Socket(s)").and_then(|value| value.parse::<usize>().ok());
    let cores_per_socket = lscpu_value(lscpu, "Core(s) per socket")
        .and_then(|value| value.parse::<usize>().ok());
    let threads_per_core = lscpu_value(lscpu, "Thread(s) per core")
        .and_then(|value| value.parse::<usize>().ok());

    let mut parts = vec![format!("{} logical CPU{}", logical, if logical == 1 { "" } else { "s" })];
    match (cores_per_socket, threads_per_core) {
        (Some(cores), Some(threads)) if cores > 0 && threads > 0 => {
            parts.push(format!("{} cores × {} thread{}", cores, threads, if threads == 1 { "" } else { "s" }));
        }
        (Some(cores), _) if cores > 0 => parts.push(format!("{} cores/package", cores)),
        _ => {}
    }
    if let Some(packages) = sockets.filter(|value| *value > 0) {
        parts.push(format!("{} package{}", packages, if packages == 1 { "" } else { "s" }));
    }
    if !architecture.is_empty() {
        parts.push(architecture.to_string());
    }
    parts.join(" • ")
}

fn lscpu_value(output: &str, key: &str) -> Option<String> {
    output.lines().find_map(|line| value_after_colon(line, key))
}

fn arm_cpu_fallback(implementer: Option<&str>, part: Option<&str>) -> String {
    let vendor = match implementer.unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "0x41" => "Arm",
        "0x42" => "Broadcom",
        "0x43" => "Cavium",
        "0x46" => "Fujitsu",
        "0x48" => "HiSilicon",
        "0x4e" => "NVIDIA",
        "0x50" => "AppliedMicro",
        "0x51" => "Qualcomm",
        "0x53" => "Samsung",
        "0x61" => "Apple",
        "0x69" => "Intel",
        _ => "ARM",
    };
    let part = part.unwrap_or_default().trim().to_ascii_lowercase();
    let core = match part.as_str() {
        "0xd03" => "Cortex-A53",
        "0xd04" => "Cortex-A35",
        "0xd05" => "Cortex-A55",
        "0xd07" => "Cortex-A57",
        "0xd08" => "Cortex-A72",
        "0xd09" => "Cortex-A73",
        "0xd0a" => "Cortex-A75",
        "0xd0b" => "Cortex-A76",
        "0xd0c" => "Neoverse-N1",
        "0xd40" => "Neoverse-V1",
        "0xd41" => "Cortex-A78",
        "0xd44" => "Cortex-X1",
        "0xd46" => "Cortex-A510",
        "0xd47" => "Cortex-A710",
        "0xd48" => "Cortex-X2",
        "0xd49" => "Neoverse-N2",
        "0xd4b" => "Cortex-A78C",
        "0xd4d" => "Cortex-A715",
        "0xd4e" => "Cortex-X3",
        "0xd80" => "Cortex-A520",
        "0xd81" => "Cortex-A720",
        "0xd82" => "Cortex-X4",
        _ => "",
    };
    if !core.is_empty() {
        format!("{} {}", vendor, core)
    } else if !part.is_empty() {
        format!("{} CPU ({})", vendor, part)
    } else {
        format!("{} CPU", vendor)
    }
}

fn concise_device_tree_platform(output: &str) -> String {
    let compatible = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if compatible.is_empty() {
        return String::new();
    }
    let (vendor, model) = compatible
        .split_once(',')
        .unwrap_or(("", compatible));
    let vendor = match vendor {
        "qcom" => "Qualcomm",
        "rockchip" => "Rockchip",
        "mediatek" => "MediaTek",
        "nvidia" => "NVIDIA",
        "brcm" => "Broadcom",
        "apple" => "Apple",
        "amlogic" => "Amlogic",
        "allwinner" => "Allwinner",
        "nxp" | "fsl" => "NXP",
        other => other,
    };
    let model = model
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    join_non_empty(&[vendor.to_string(), model], " ")
}

fn split_sections(output: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut current = String::new();
    for line in output.lines() {
        if let Some(marker) = line
            .strip_prefix("@@")
            .and_then(|value| value.strip_suffix("@@"))
        {
            current = marker.to_string();
            result.entry(current.clone()).or_insert_with(String::new);
        } else if !current.is_empty() {
            let entry = result.entry(current.clone()).or_insert_with(String::new);
            entry.push_str(line);
            entry.push('\n');
        }
    }
    result
}

fn section<'a>(sections: &'a HashMap<String, String>, name: &str) -> &'a str {
    sections.get(name).map(String::as_str).unwrap_or_default()
}

fn parse_disk_usage(output: &str) -> Vec<DiskUsage> {
    let mut grouped = HashMap::<String, DiskUsage>::new();
    let mut seen_filesystems = HashSet::<(String, String)>::new();

    for line in output.lines() {
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() < 9 {
            continue;
        }

        let root = parts[0].trim();
        let vendor = parts[1].trim();
        let model = parts[2].trim();
        let drive_size = parts[3].trim().parse::<u64>().unwrap_or_default();
        let source = parts[4].trim();
        let filesystem_total = parts[6].trim().parse::<u64>().unwrap_or_default();
        let used_bytes = parts[7].trim().parse::<u64>().unwrap_or_default();
        if root.is_empty() || source.is_empty() || drive_size == 0 {
            continue;
        }

        // A filesystem can be mounted more than once (notably Btrfs subvolumes).
        // Count it once, then combine every mounted filesystem that belongs to
        // the same physical disk into one drive-level card.
        let key = (root.to_string(), source.to_string());
        if !seen_filesystems.insert(key) {
            continue;
        }

        let name = join_non_empty(&[vendor.to_string(), model.to_string()], " ");
        let entry = grouped.entry(root.to_string()).or_insert_with(|| DiskUsage {
            name,
            device: format!("/dev/{root}"),
            total_bytes: drive_size,
            used_bytes: 0,
            available_bytes: drive_size,
            temperature_c: None,
        });
        entry.total_bytes = entry.total_bytes.max(drive_size).max(filesystem_total);
        entry.used_bytes = entry.used_bytes.saturating_add(used_bytes);
        entry.used_bytes = entry.used_bytes.min(entry.total_bytes);
        entry.available_bytes = entry.total_bytes.saturating_sub(entry.used_bytes);
    }

    let mut disks = grouped.into_values().collect::<Vec<_>>();
    disks.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.device.cmp(&right.device))
    });
    disks
}

fn collect_disk_temperature(root: &str) -> Option<f64> {
    let mut roots = Vec::<PathBuf>::new();
    roots.push(Path::new("/sys/class/block").join(root).join("device/hwmon"));
    roots.push(Path::new("/sys/block").join(root).join("device/hwmon"));

    if let Some(controller) = nvme_controller_name(root) {
        roots.push(
            Path::new("/sys/class/nvme")
                .join(controller)
                .join("device/hwmon"),
        );
    }

    if let Ok(canonical) = fs::canonicalize(Path::new("/sys/class/block").join(root)) {
        for ancestor in canonical.ancestors().take(7) {
            roots.push(ancestor.join("hwmon"));
        }
    }

    let mut seen = HashSet::<PathBuf>::new();
    let mut values = Vec::new();
    for hwmon_root in roots {
        let canonical = fs::canonicalize(&hwmon_root).unwrap_or(hwmon_root);
        if !seen.insert(canonical.clone()) {
            continue;
        }
        for hwmon in read_dir(&canonical) {
            for entry in read_dir(hwmon.path()) {
                let filename = entry.file_name().to_string_lossy().into_owned();
                if !filename.starts_with("temp") || !filename.ends_with("_input") {
                    continue;
                }
                if let Some(raw) = read_number(entry.path()) {
                    let value = normalize_temperature(raw);
                    if (-20.0..=200.0).contains(&value) {
                        values.push(value);
                    }
                }
            }
        }
    }

    values.into_iter().reduce(f64::max)
}

fn nvme_controller_name(root: &str) -> Option<&str> {
    let suffix = root.strip_prefix("nvme")?;
    let namespace_separator = suffix.find('n')?;
    Some(&root[..("nvme".len() + namespace_separator)])
}

fn parse_pci_inventory(output: &str) -> (Vec<ComponentInfo>, Vec<ComponentInfo>) {
    let mut graphics = Vec::new();
    let mut other = Vec::new();

    for line in output.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (address, description) = line.split_once(' ').unwrap_or(("", line));
        let lower = description.to_ascii_lowercase();
        let component = ComponentInfo {
            name: clean_pci_description(description),
            details: address.to_string(),
        };
        if lower.contains("vga compatible controller")
            || lower.contains("3d controller")
            || lower.contains("display controller")
        {
            graphics.push(component);
        } else {
            other.push(component);
        }
    }

    (graphics, other)
}

fn clean_pci_description(description: &str) -> String {
    let trimmed = description.trim();
    if let Some((prefix, suffix)) = trimmed.rsplit_once(" [") {
        let id = suffix.trim_end_matches(']');
        if id.contains(':') && id.chars().all(|ch| ch.is_ascii_hexdigit() || ch == ':') {
            return prefix.to_string();
        }
    }
    trimmed.to_string()
}

fn parse_lsusb(output: &str) -> Vec<ComponentInfo> {
    output
        .lines()
        .filter_map(|line| {
            let (location, rest) = line.split_once(": ID ")?;
            let (id, name) = rest.split_once(' ').unwrap_or((rest, "USB device"));
            Some(ComponentInfo {
                name: name.trim().to_string(),
                details: format!("{} • ID {}", location.trim(), id.trim()),
            })
        })
        .collect()
}

fn collect_battery_components() -> Vec<ComponentInfo> {
    read_dir("/sys/class/power_supply")
        .into_iter()
        .filter_map(|supply| {
            let path = supply.path();
            if read_trim(path.join("type")) != "Battery" || is_device_battery(&path) {
                return None;
            }
            let manufacturer = read_trim(path.join("manufacturer"));
            let model = read_trim(path.join("model_name"));
            let kernel_name = supply.file_name().to_string_lossy().into_owned();
            let name = join_non_empty(&[manufacturer, model], " ");
            Some(ComponentInfo {
                name: if name.is_empty() { kernel_name } else { name },
                details: "Battery".to_string(),
            })
        })
        .collect()
}

fn parse_cpu_times(stat: &str) -> Option<CpuTimes> {
    let values = stat
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect::<Vec<_>>();
    if values.len() < 4 {
        return None;
    }
    let idle = values.get(3).copied().unwrap_or_default()
        + values.get(4).copied().unwrap_or_default();
    let total = values.iter().sum();
    Some(CpuTimes { idle, total })
}

#[derive(Default)]
struct MemoryInfo {
    total: u64,
    available: u64,
}

fn parse_meminfo(meminfo: &str) -> MemoryInfo {
    let mut values = HashMap::new();
    for line in meminfo.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            let kib = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default();
            values.insert(key, kib.saturating_mul(1024));
        }
    }
    let total = values.get("MemTotal").copied().unwrap_or_default();
    let available = values
        .get("MemAvailable")
        .copied()
        .unwrap_or_else(|| {
            values.get("MemFree").copied().unwrap_or_default()
                + values.get("Buffers").copied().unwrap_or_default()
                + values.get("Cached").copied().unwrap_or_default()
        });
    MemoryInfo { total, available }
}

fn collect_cpu_frequency_mhz() -> Option<f64> {
    let cpu_root = Path::new("/sys/devices/system/cpu");
    let mut values = Vec::new();
    for entry in read_dir(cpu_root) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("cpu") || !name[3..].chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let frequency = read_number(entry.path().join("cpufreq/scaling_cur_freq"))
            .or_else(|| read_number(entry.path().join("cpufreq/cpuinfo_cur_freq")));
        if let Some(khz) = frequency {
            values.push(khz / 1000.0);
        }
    }
    if !values.is_empty() {
        return Some(values.iter().sum::<f64>() / values.len() as f64);
    }

    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|cpuinfo| {
            let mhz = cpuinfo
                .lines()
                .filter_map(|line| value_after_colon(line, "cpu MHz"))
                .filter_map(|value| value.parse::<f64>().ok())
                .collect::<Vec<_>>();
            (!mhz.is_empty()).then(|| mhz.iter().sum::<f64>() / mhz.len() as f64)
        })
}

fn collect_hwmon_sensors() -> Vec<SensorReading> {
    let mut sensors = Vec::new();
    for hwmon in read_dir("/sys/class/hwmon") {
        let path = hwmon.path();
        let chip = read_trim(path.join("name"));
        let chip = if chip.is_empty() {
            hwmon.file_name().to_string_lossy().into_owned()
        } else {
            chip
        };

        let files = read_dir(&path)
            .into_iter()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();

        for file in &files {
            let filename = file
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();

            if filename.starts_with("temp") && filename.ends_with("_input") {
                let stem = filename.trim_end_matches("_input");
                if let Some(raw) = read_number(file) {
                    let value = normalize_temperature(raw);
                    if !(-20.0..=200.0).contains(&value) {
                        continue;
                    }
                    let label = read_trim(path.join(format!("{stem}_label")));
                    let display = if label.is_empty() {
                        format!("{chip} {stem}")
                    } else {
                        format!("{chip}: {label}")
                    };
                    sensors.push(SensorReading {
                        kind: classify_temperature(&chip, &label).to_string(),
                        name: display,
                        value,
                        unit: "°C".to_string(),
                    });
                }
            } else if filename.starts_with("fan") && filename.ends_with("_input") {
                let stem = filename.trim_end_matches("_input");
                if let Some(value) = read_number(file) {
                    let label = read_trim(path.join(format!("{stem}_label")));
                    sensors.push(SensorReading {
                        kind: "Fan".to_string(),
                        name: if label.is_empty() {
                            format!("{chip} {stem}")
                        } else {
                            format!("{chip}: {label}")
                        },
                        value,
                        unit: "RPM".to_string(),
                    });
                }
            } else if filename.starts_with("power")
                && (filename.ends_with("_average") || filename.ends_with("_input"))
            {
                let stem = filename
                    .trim_end_matches("_average")
                    .trim_end_matches("_input");
                if filename.ends_with("_input")
                    && path.join(format!("{stem}_average")).exists()
                {
                    continue;
                }
                if let Some(raw) = read_number(file) {
                    sensors.push(SensorReading {
                        kind: "Power".to_string(),
                        name: format!("{chip} {stem}"),
                        value: raw / 1_000_000.0,
                        unit: "W".to_string(),
                    });
                }
            }
        }
    }

    sensors.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.name.cmp(&b.name))
    });
    sensors.dedup_by(|a, b| a.kind == b.kind && a.name == b.name);
    sensors
}

fn collect_thermal_zone_sensors() -> Vec<SensorReading> {
    let mut sensors = Vec::new();
    for zone in read_dir("/sys/class/thermal") {
        let name = zone.file_name().to_string_lossy().into_owned();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let path = zone.path();
        let zone_type = read_trim(path.join("type"));
        let Some(raw) = read_number(path.join("temp")) else {
            continue;
        };
        let value = normalize_temperature(raw);
        if !(-20.0..=200.0).contains(&value) {
            continue;
        }
        let display_name = if zone_type.is_empty() {
            name
        } else {
            zone_type.replace('_', " ").replace('-', " ")
        };
        sensors.push(SensorReading {
            kind: classify_temperature("thermal-zone", &display_name).to_string(),
            name: display_name,
            value,
            unit: "°C".to_string(),
        });
    }
    sensors
}

fn classify_temperature(chip: &str, label: &str) -> &'static str {
    let combined = format!("{} {}", chip, label).to_ascii_lowercase();
    if [
        "coretemp",
        "k10temp",
        "zenpower",
        "cpu",
        "package",
        "core",
        "soc-thermal",
        "soc_thermal",
        "soc thermal",
        "cpu-thermal",
        "cpu_thermal",
        "cpu thermal",
        "big-thermal",
        "big thermal",
        "little-thermal",
        "little thermal",
        "cluster",
    ]
        .iter()
        .any(|needle| combined.contains(needle))
    {
        "CPU temperature"
    } else if [
        "amdgpu",
        "nouveau",
        "nvidia",
        "gpu",
        "junction",
        "vram",
        "panfrost",
        "panthor",
        "lima",
        "adreno",
        "v3d",
        "vc4",
        "etnaviv",
    ]
        .iter()
        .any(|needle| combined.contains(needle))
    {
        "GPU temperature"
    } else if ["dimm", "dram", "ddr", "spd", "memory"]
        .iter()
        .any(|needle| combined.contains(needle))
    {
        "Memory temperature"
    } else if ["nvme", "ssd", "hdd", "drive", "composite"]
        .iter()
        .any(|needle| combined.contains(needle))
    {
        "Storage temperature"
    } else {
        "Motherboard temperature"
    }
}

fn normalize_temperature(raw: f64) -> f64 {
    if raw.abs() > 1000.0 {
        raw / 1000.0
    } else {
        raw
    }
}

fn parse_gpu_descriptors(output: &str) -> HashMap<String, GpuDescriptor> {
    let mut descriptors = HashMap::new();

    for block in output.split("\n\n") {
        let mut fields = HashMap::<&str, &str>::new();
        for line in block.lines() {
            if let Some((key, value)) = line.split_once(':') {
                fields.insert(key.trim(), value.trim());
            }
        }

        let class = fields.get("Class").copied().unwrap_or_default();
        let class_lower = class.to_ascii_lowercase();
        if !class_lower.contains("vga compatible controller")
            && !class_lower.contains("3d controller")
            && !class_lower.contains("display controller")
        {
            continue;
        }

        let slot = normalize_pci_address(fields.get("Slot").copied().unwrap_or_default());
        if slot.is_empty() {
            continue;
        }

        let vendor = clean_pci_field(fields.get("Vendor").copied().unwrap_or_default());
        let device = clean_pci_field(fields.get("Device").copied().unwrap_or_default());
        let name = join_non_empty(&[vendor, device], " ");
        let board_vendor = clean_board_vendor(&clean_pci_field(
            fields.get("SVendor").copied().unwrap_or_default(),
        ));
        let board_model = clean_pci_field(fields.get("SDevice").copied().unwrap_or_default());
        let board_name = build_board_name(&board_vendor, &board_model);

        descriptors.insert(slot, GpuDescriptor { name, board_name });
    }

    descriptors
}

fn parse_nvidia_gpus(output: &str) -> Vec<GpuInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
            if parts.len() < 8 || parts[1].is_empty() {
                return None;
            }
            Some(GpuInfo {
                name: parts[1].to_string(),
                board_name: None,
                bus_id: Some(normalize_pci_address(parts[0])),
                usage_percent: parse_optional_number(parts[2]),
                temperature_c: parse_optional_number(parts[3]),
                memory_used_bytes: parse_optional_number(parts[4])
                    .map(|value| (value * 1024.0 * 1024.0) as u64),
                memory_total_bytes: parse_optional_number(parts[5])
                    .map(|value| (value * 1024.0 * 1024.0) as u64),
                power_watts: parse_optional_number(parts[6]),
                fan_percent: parse_optional_number(parts[7]),
            })
        })
        .collect()
}

fn collect_sysfs_gpus(
    descriptors: &HashMap<String, GpuDescriptor>,
    nvidia_gpus: &[GpuInfo],
) -> Vec<GpuInfo> {
    let mut result = nvidia_gpus.to_vec();
    for card in read_dir("/sys/class/drm") {
        let card_name = card.file_name().to_string_lossy().into_owned();
        if !card_name.starts_with("card")
            || !card_name[4..].chars().all(|character| character.is_ascii_digit())
        {
            continue;
        }
        let device = card.path().join("device");
        if !device.exists() {
            continue;
        }
        let vendor = read_trim(device.join("vendor"));
        if vendor.eq_ignore_ascii_case("0x10de") && !nvidia_gpus.is_empty() {
            continue;
        }
        let bdf = fs::canonicalize(&device)
            .ok()
            .and_then(|path| path.file_name().map(|value| value.to_string_lossy().into_owned()))
            .map(|value| normalize_pci_address(&value))
            .unwrap_or_default();
        let driver = drm_driver_name(&device);
        let compatible = read_nul_separated(device.join("of_node/compatible"));
        let fallback_name = gpu_name_from_sysfs(&vendor, &driver, &compatible, &card_name);
        let descriptor = descriptors.get(&bdf);
        let name = descriptor
            .map(|descriptor| descriptor.name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(fallback_name);
        let temperature_c = first_number_under(&device.join("hwmon"), "temp1_input")
            .map(normalize_temperature);
        let power_watts = first_number_under(&device.join("hwmon"), "power1_average")
            .map(|value| value / 1_000_000.0);
        result.push(GpuInfo {
            name,
            board_name: descriptor.and_then(|descriptor| descriptor.board_name.clone()),
            bus_id: (!bdf.is_empty()).then_some(bdf),
            usage_percent: read_number(device.join("gpu_busy_percent"))
                .or_else(|| read_number(device.join("busy_percent")))
                .or_else(|| read_devfreq_load(&device)),
            temperature_c,
            memory_used_bytes: read_number(device.join("mem_info_vram_used"))
                .map(|value| value as u64),
            memory_total_bytes: read_number(device.join("mem_info_vram_total"))
                .map(|value| value as u64),
            power_watts,
            fan_percent: None,
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

fn drm_driver_name(device: &Path) -> String {
    for candidate in [device.join("driver/module"), device.join("driver")] {
        if let Ok(path) = fs::canonicalize(candidate) {
            if let Some(name) = path.file_name() {
                return name.to_string_lossy().into_owned();
            }
        }
    }
    read_trim(device.join("uevent"))
        .lines()
        .find_map(|line| line.strip_prefix("DRIVER="))
        .unwrap_or_default()
        .to_string()
}

fn read_nul_separated(path: impl AsRef<Path>) -> Vec<String> {
    fs::read(path)
        .ok()
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter_map(|part| String::from_utf8(part.to_vec()).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn gpu_name_from_sysfs(vendor: &str, driver: &str, compatible: &[String], card_name: &str) -> String {
    let vendor_lower = vendor.to_ascii_lowercase();
    let driver_lower = driver.to_ascii_lowercase();
    let compatible_lower = compatible.join(" ").to_ascii_lowercase();
    let base = match vendor_lower.as_str() {
        "0x1002" => "AMD GPU",
        "0x8086" => "Intel GPU",
        "0x10de" => "NVIDIA GPU",
        _ if driver_lower.contains("panthor") || driver_lower.contains("panfrost") => "Arm Mali GPU",
        _ if driver_lower.contains("lima") => "Arm Mali-4xx GPU",
        _ if driver_lower == "msm" || compatible_lower.contains("adreno") => "Qualcomm Adreno GPU",
        _ if driver_lower.contains("etnaviv") => "Vivante GPU",
        _ if driver_lower == "v3d" => "Broadcom V3D GPU",
        _ if driver_lower == "vc4" => "Broadcom VideoCore GPU",
        _ if driver_lower.contains("apple") || compatible_lower.contains("apple,gpu") => "Apple GPU",
        _ if driver_lower.contains("tegra") => "NVIDIA Tegra GPU",
        _ if driver_lower.contains("pvr") || compatible_lower.contains("powervr") => "PowerVR GPU",
        _ if compatible_lower.contains("mali") => "Arm Mali GPU",
        _ => "Graphics adapter",
    };
    if driver.is_empty() {
        format!("{} ({})", base, card_name)
    } else {
        format!("{} · {}", base, driver)
    }
}

fn read_devfreq_load(device: &Path) -> Option<f64> {
    let devfreq_root = device.join("devfreq");
    for entry in read_dir(&devfreq_root) {
        let load = read_trim(entry.path().join("load"));
        let values = load
            .split_whitespace()
            .filter_map(|value| value.parse::<f64>().ok())
            .collect::<Vec<_>>();
        match values.as_slice() {
            [percent] if (0.0..=100.0).contains(percent) => return Some(*percent),
            [busy, total, ..] if *total > 0.0 => return Some((100.0 * busy / total).clamp(0.0, 100.0)),
            _ => {}
        }
    }
    None
}

fn normalize_pci_address(value: &str) -> String {
    let value = value.trim();
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() == 3 {
        let domain = parts[0];
        let domain = if domain.len() > 4 {
            &domain[domain.len() - 4..]
        } else {
            domain
        };
        format!("{}:{}:{}", domain, parts[1], parts[2]).to_ascii_lowercase()
    } else if parts.len() == 2 {
        format!("0000:{}:{}", parts[0], parts[1]).to_ascii_lowercase()
    } else {
        value.to_ascii_lowercase()
    }
}

fn clean_pci_field(value: &str) -> String {
    let mut result = value.trim().to_string();
    loop {
        let Some((prefix, suffix)) = result.rsplit_once(" [") else {
            break;
        };
        let id = suffix.trim_end_matches(']');
        if id.is_empty() || !id.chars().all(|character| character.is_ascii_hexdigit() || character == ':') {
            break;
        }
        result = prefix.trim().to_string();
    }
    result
}

fn clean_board_vendor(value: &str) -> String {
    let mut value = value.to_string();
    for suffix in [
        " Technology Limited",
        " Technology Co., Ltd.",
        " Co., Ltd.",
        " Corporation",
        " Incorporated",
        " Inc.",
        " Ltd.",
    ] {
        value = value.replace(suffix, "");
    }
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn build_board_name(vendor: &str, model: &str) -> Option<String> {
    let vendor = vendor.trim();
    let model = concise_board_model(model);

    match (vendor.is_empty(), model.is_empty()) {
        (true, true) => None,
        (false, true) => Some(vendor.to_string()),
        (true, false) => Some(model),
        (false, false) if model.to_ascii_lowercase().contains(&vendor.to_ascii_lowercase()) => {
            Some(model)
        }
        (false, false) => Some(format!("{vendor} {model}")),
    }
}

fn concise_board_model(model: &str) -> String {
    let model = model.trim();
    let lower = model.to_ascii_lowercase();
    if model.is_empty()
        || lower == "device"
        || lower.starts_with("device ")
        || lower == "unknown"
    {
        return String::new();
    }

    let cut = [
        " amd ", "amd radeon", " radeon", " nvidia ", "nvidia geforce",
        " geforce", " intel ", "intel arc", " arc ", " rtx", " gtx",
    ]
    .iter()
    .filter_map(|needle| lower.find(needle))
    .min();

    let candidate = cut
        .map(|index| model[..index].trim_matches(|character: char| {
            character.is_whitespace() || character == '-' || character == '_'
        }))
        .filter(|value| !value.is_empty())
        .unwrap_or(model);

    candidate.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_number_under(root: &Path, filename: &str) -> Option<f64> {
    for entry in read_dir(root) {
        let candidate = entry.path().join(filename);
        if let Some(value) = read_number(candidate) {
            return Some(value);
        }
    }
    None
}

fn collect_connected_devices(runner: &HostRunner) -> Vec<DeviceInfo> {
    let output = runner.run_script(DEVICE_SCRIPT);
    let mut devices = Vec::<DeviceCandidate>::new();
    let mut batteries = Vec::<(String, String, f64)>::new();

    for line in output.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        match fields.first().copied() {
            Some("D") if fields.len() >= 6 => {
                let kind = fields[1].trim();
                let name = fields[3].trim();
                if !matches!(kind, "Keyboard" | "Mouse" | "Controller" | "Headset")
                    || !meaningful_device_name(name)
                {
                    continue;
                }
                let battery_percent = fields[5]
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|value| (0.0..=100.0).contains(value));
                let physical_id = fields[2].trim();
                let identity = if physical_id.is_empty() {
                    normalize_device_name(name)
                } else {
                    physical_id.to_ascii_lowercase()
                };
                let kind_confidence = fields
                    .get(6)
                    .and_then(|value| value.trim().parse::<u8>().ok())
                    .unwrap_or_else(|| device_kind_priority(kind, name));
                devices.push(DeviceCandidate {
                    info: DeviceInfo {
                        key: format!("{identity}|{}", normalize_device_name(name)),
                        name: name.to_string(),
                        kind: kind.to_string(),
                        transport: fields[4].trim().to_string(),
                        battery_percent,
                    },
                    kind_confidence,
                });
            }
            Some("B") if fields.len() >= 3 => {
                if let Ok(value) = fields[2].trim().parse::<f64>() {
                    if (0.0..=100.0).contains(&value) {
                        batteries.push((
                            fields[1].trim().to_string(),
                            fields.get(3).map(|value| value.trim()).unwrap_or_default().to_ascii_lowercase(),
                            value,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    for candidate in &mut devices {
        let device = &mut candidate.info;
        if device.battery_percent.is_none() {
            device.battery_percent = batteries
                .iter()
                .filter(|(name, identity, _)| {
                    (!identity.is_empty() && same_physical_device(&device.key, identity))
                        || device_names_match(&device.name, name)
                })
                .map(|(_, _, value)| *value)
                .next();
        }

        // Receiver-backed HID devices often expose no battery and may use a
        // generic USB product string. Use receiver IDs and well-known wireless
        // model language in addition to battery presence.
        if device.transport == "Wired USB"
            && (device.battery_percent.is_some()
                || usb_identity_is_known_receiver(&device.key)
                || device_name_suggests_wireless(&device.name))
        {
            device.transport = "2.4 GHz wireless".to_string();
        }
    }

    let mut deduplicated = Vec::<DeviceCandidate>::new();
    for candidate in devices {
        if let Some(existing) = deduplicated.iter_mut().find(|existing| {
            let existing_info = &existing.info;
            let device = &candidate.info;
            (same_physical_device(&existing_info.key, &device.key)
                && (device_names_match(&existing_info.name, &device.name)
                    || existing_info.kind == device.kind
                    || is_auxiliary_interface_name(&existing_info.name)
                    || is_auxiliary_interface_name(&device.name)))
                || (existing_info.kind == device.kind
                    && device_names_match(&existing_info.name, &device.name)
                    && (existing_info.transport == "Bluetooth" || device.transport == "Bluetooth"))
        }) {
            let device = &candidate.info;
            if existing.info.battery_percent.is_none() {
                existing.info.battery_percent = device.battery_percent;
            }
            if transport_priority(&device.transport) > transport_priority(&existing.info.transport) {
                existing.info.transport = device.transport.clone();
            }
            if candidate.kind_confidence > existing.kind_confidence {
                existing.info.kind = device.kind.clone();
                existing.kind_confidence = candidate.kind_confidence;
            }
            if device_name_priority(&device.name) > device_name_priority(&existing.info.name) {
                existing.info.name = device.name.clone();
            }
            continue;
        }
        deduplicated.push(candidate);
    }

    let mut deduplicated = deduplicated
        .into_iter()
        .map(|candidate| candidate.info)
        .collect::<Vec<_>>();
    deduplicated.sort_by(|left, right| {
        device_kind_order(&left.kind)
            .cmp(&device_kind_order(&right.kind))
            .then_with(|| left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase()))
    });
    deduplicated

}

fn usb_identity_is_known_receiver(key: &str) -> bool {
    let identity = key
        .split_once('|')
        .map(|(identity, _)| identity)
        .unwrap_or(key)
        .to_ascii_lowercase();

    // Logitech receiver products conventionally use the c5xx range. Keep
    // exact common receiver IDs for clarity and accept that range as a fallback.
    if let Some(rest) = identity.strip_prefix("usb:046d:") {
        let product = rest.split(':').next().unwrap_or_default();
        if product.starts_with("c5") {
            return true;
        }
    }

    [
        "usb:045e:0719", // Microsoft Xbox wireless adapter
        "usb:045e:02e6",
        "usb:045e:02fe",
        "usb:054c:0ce6", // Sony wireless controller adapters/dongles
        "usb:1038:12ad", // SteelSeries receiver families seen in the wild
    ]
    .iter()
    .any(|prefix| identity.starts_with(prefix))
}

fn device_name_suggests_wireless(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "2.4g",
        "2.4 ghz",
        "wireless",
        "receiver",
        "dongle",
        "unifying",
        "lightspeed",
        "bolt",
        "cordless",
        "superlight",
        "mx master",
        "mx anywhere",
        "g305",
        "g304",
        "g603",
        "g703",
        "g903",
        "viper v2 pro",
        "viper v3 pro",
        "deathadder v3 pro",
        "basilisk v3 pro",
        "aerox",
        "prime wireless",
        "model o wireless",
        "model d wireless",
        "kone pro air",
        "burst pro air",
        "harpe ace",
        "keris wireless",
        "lamzu",
        "pulsar",
        "ninjutso",
        "zaopin",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn same_physical_device(left_key: &str, right_key: &str) -> bool {
    let left = left_key.split_once('|').map(|(identity, _)| identity).unwrap_or(left_key);
    let right = right_key
        .split_once('|')
        .map(|(identity, _)| identity)
        .unwrap_or(right_key);
    !left.is_empty() && left == right
}

fn is_auxiliary_interface_name(name: &str) -> bool {
    let normalized = normalize_device_name(name);
    normalized.is_empty()
        || [
            "consumer control",
            "system control",
            "usb receiver",
            "hid device",
            "input device",
            "keyboard",
            "mouse",
            "receiver",
        ]
        .iter()
        .any(|generic| normalized == *generic || normalized.ends_with(generic))
}

fn device_kind_priority(kind: &str, name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    let explicit = match kind {
        "Controller" if lower.contains("controller") || lower.contains("gamepad") || lower.contains("joystick") => 180,
        "Headset" if lower.contains("headset") || lower.contains("headphone") || lower.contains("earbud") => 180,
        "Mouse" if lower.contains("mouse") || lower.contains("trackball") => 180,
        "Keyboard" if lower.contains("keyboard") || lower.contains("keypad") => 180,
        _ => 0,
    };
    explicit
        + match kind {
            "Controller" => 40,
            "Headset" => 30,
            "Mouse" => 20,
            "Keyboard" => 10,
            _ => 0,
        }
}

fn transport_priority(transport: &str) -> u8 {
    match transport {
        "Bluetooth" => 4,
        "2.4 GHz wireless" => 3,
        "Built-in" => 2,
        "Wired USB" | "Wired" => 1,
        _ => 0,
    }
}

fn device_name_priority(name: &str) -> usize {
    let normalized = normalize_device_name(name);
    normalized.split_whitespace().count() * 100 + normalized.len()
}

fn device_kind_order(kind: &str) -> u8 {
    match kind {
        "Keyboard" => 0,
        "Mouse" => 1,
        "Controller" => 2,
        "Headset" => 3,
        _ => 4,
    }
}

fn meaningful_device_name(name: &str) -> bool {
    let normalized = normalize_device_name(name);
    !normalized.is_empty()
        && !matches!(
            normalized.as_str(),
            "unknown" | "device" | "usb device" | "input device" | "default string"
        )
        && !normalized.contains("power button")
        && !normalized.contains("sleep button")
        && !normalized.contains("video bus")
}

fn normalize_device_name(name: &str) -> String {
    let raw = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let filtered = raw
        .split_whitespace()
        .filter(|word| {
            !matches!(
                *word,
                "usb" | "bluetooth" | "wireless" | "gaming" | "device" | "receiver"
                    | "keyboard" | "mouse" | "headset" | "controller" | "battery"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    if filtered.is_empty() { raw } else { filtered }
}

fn device_names_match(left: &str, right: &str) -> bool {
    let left = normalize_device_name(left);
    let right = normalize_device_name(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right
        || (left.len() >= 4 && right.contains(&left))
        || (right.len() >= 4 && left.contains(&right))
    {
        return true;
    }

    left.split_whitespace().any(|word| {
        word.len() >= 4
            && word.chars().any(|character| character.is_ascii_digit())
            && right.split_whitespace().any(|other| word == other)
    })
}

fn is_device_battery(path: &Path) -> bool {
    if read_trim(path.join("scope")).eq_ignore_ascii_case("device") {
        return true;
    }
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.contains("hidpp") || name.starts_with("hid-") || name.contains("bluetooth")
}

fn collect_batteries() -> Vec<BatteryInfo> {
    let mut batteries = Vec::new();
    for supply in read_dir("/sys/class/power_supply") {
        let path = supply.path();
        if read_trim(path.join("type")) != "Battery" || is_device_battery(&path) {
            continue;
        }
        let manufacturer = read_trim(path.join("manufacturer"));
        let model = read_trim(path.join("model_name"));
        let kernel_name = supply.file_name().to_string_lossy().into_owned();
        let name = join_non_empty(&[manufacturer, model], " ");
        let power_watts = read_number(path.join("power_now"))
            .map(|value| value / 1_000_000.0)
            .or_else(|| {
                let current = read_number(path.join("current_now"))?;
                let voltage = read_number(path.join("voltage_now"))?;
                Some(current * voltage / 1_000_000_000_000.0)
            });
        batteries.push(BatteryInfo {
            name: if name.is_empty() { kernel_name } else { name },
            status: read_trim(path.join("status")),
            capacity_percent: read_number(path.join("capacity")),
            power_watts,
            energy_full_wh: read_number(path.join("energy_full"))
                .map(|value| value / 1_000_000.0)
                .or_else(|| {
                    let charge = read_number(path.join("charge_full"))?;
                    let voltage = read_number(path.join("voltage_min_design"))
                        .or_else(|| read_number(path.join("voltage_now")))?;
                    Some(charge * voltage / 1_000_000_000_000.0)
                }),
        });
    }
    batteries
}

fn parse_lsblk(output: &str) -> Vec<ComponentInfo> {
    let mut result = Vec::new();
    for line in output.lines() {
        let pairs = parse_quoted_pairs(line);
        let kind = pairs.get("TYPE").map(String::as_str).unwrap_or_default();
        if kind != "disk" && kind != "rom" {
            continue;
        }
        let name = pairs.get("NAME").cloned().unwrap_or_default();
        let model = pairs.get("MODEL").cloned().unwrap_or_default();
        let display = if model.is_empty() {
            name.clone()
        } else {
            model
        };
        let mut details = Vec::new();
        if let Some(size) = pairs.get("SIZE").filter(|value| !value.is_empty()) {
            details.push(size.clone());
        }
        if let Some(transport) = pairs.get("TRAN").filter(|value| !value.is_empty()) {
            details.push(transport.to_ascii_uppercase());
        }
        if let Some(rotational) = pairs.get("ROTA") {
            details.push(if rotational == "1" {
                "rotational".to_string()
            } else {
                "solid-state".to_string()
            });
        }
        details.push(format!("/dev/{}", name));
        result.push(ComponentInfo {
            name: display,
            details: details.join(" • "),
        });
    }
    result
}

fn parse_quoted_pairs(line: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let key_start = index;
        while index < bytes.len() && bytes[index] != b'=' {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let key = &line[key_start..index];
        index += 1;
        if index >= bytes.len() || bytes[index] != b'"' {
            break;
        }
        index += 1;
        let mut value = String::new();
        while index < bytes.len() {
            if bytes[index] == b'"' {
                index += 1;
                break;
            }
            if bytes[index] == b'\\' && index + 1 < bytes.len() {
                index += 1;
            }
            value.push(bytes[index] as char);
            index += 1;
        }
        result.insert(key.to_string(), value);
    }
    result
}

fn value_after_colon(line: &str, wanted_key: &str) -> Option<String> {
    let (key, value) = line.split_once(':')?;
    (key.trim() == wanted_key).then(|| value.trim().to_string())
}

fn parse_optional_number(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("n/a") {
        None
    } else {
        trimmed.parse::<f64>().ok()
    }
}

fn read_trim(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .map(|value| value.trim_matches(char::from(0)).trim().to_string())
        .unwrap_or_default()
}

fn read_number(path: impl AsRef<Path>) -> Option<f64> {
    read_trim(path).parse::<f64>().ok()
}

fn read_dir(path: impl AsRef<Path>) -> Vec<fs::DirEntry> {
    fs::read_dir(path)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn join_non_empty(values: &[String], separator: &str) -> String {
    values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(separator)
}
